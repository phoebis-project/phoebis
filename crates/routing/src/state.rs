//! What the router task owns between selections: pins, cooldowns, the newest overage evidence per account, the
//! newest utilization sample per account with the router's own charges since it, and the in-flight counts per key
//! and per account. Every mutation reports whether it changed anything so the task knows what to write through.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, RwLock};

use phoebis_store::{
    AccountKind, AccountState, AffinityRow, BudgetPeriod, CooldownRow, CreditsRow, OverageRow,
    PinBurn, PinKey, PinRow, PoolMember, PoolRow, RekeyWrite, ResolutionRow, RoutingLoad,
    StateCause, WindowRow,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::affinity::{AffinityMap, PurgedEntry};
use crate::listing::{row_for, snapshot_date, tier_segment};
use crate::select::{affinity_ttl_ms, seconds_ms, Banded, SlowDownRead, SLOW_DOWN_WAIT_MS};
use crate::task::BurnRates;
use crate::windows::{window_for, MODEL_WINDOWS};
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
    /// The pin's entrypoint as the selection stamped it, `""` for none: a client renewal writes it back to the row.
    pub entrypoint: String,
}

/// The pin's entrypoint as the pricers take it: `None` for a pin that carries none.
pub fn entrypoint_of(pin: &PinRow) -> Option<&str> {
    Some(pin.entrypoint.as_str()).filter(|entrypoint| !entrypoint.is_empty())
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

/// Where a window's reading came from: a response head, the usage poll, the registry at start (which keeps no
/// origin), or the router's own prediction once a charge or a passed reset moved the stored reading.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WindowSource {
    Head,
    Poll,
    Predicted,
    Stored,
}

impl WindowSource {
    pub fn as_str(self) -> &'static str {
        match self {
            WindowSource::Head => "head",
            WindowSource::Poll => "poll",
            WindowSource::Predicted => "predicted",
            WindowSource::Stored => "stored",
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

/// The plan windows a model's requests draw: both shared windows, and the model's own week where it has one.
pub(crate) fn plan_windows(canonical_model: &str) -> impl Iterator<Item = &'static str> {
    let own = window_for(canonical_model);
    [FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .chain((own != SEVEN_DAY).then_some(own))
}

/// Every plan window an account can hold a reading of, in the trigger's order: both shared windows, then each model
/// week `MODEL_WINDOWS` names. The projections, the member facts and the route iterate this one list, so a week added
/// to the table is projected wherever the others are.
pub(crate) fn all_plan_windows() -> impl Iterator<Item = &'static str> {
    [FIVE_HOUR, SEVEN_DAY]
        .into_iter()
        .chain(MODEL_WINDOWS.iter().map(|(_, window)| *window))
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
    pub source: WindowSource,
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
    /// other. `cause` is stored with the state, and it and `detail` ride to the event the loop emits when the write
    /// moves the row's state.
    AccountState {
        account_id: String,
        state: AccountState,
        incarnation: u64,
        lifecycle_generation: u64,
        credential_generation: u64,
        cause: StateCause,
        detail: Option<String>,
    },
    DeleteAffinity(Vec<(String, String)>),
    DeleteCooldown(String, String),
    DeletePin(PinKey),
    /// `before` is `now` less the longest `shed_dwell_seconds` of the snapshot's pools, computed at the purge tick.
    PurgeShedRounds {
        before: i64,
    },
    /// The purge tick's sweep of window and credits rows no current holder of their id wrote.
    PurgeStaleWindows,
    /// One resolution landing, committed as one transaction so the store never holds moved rows without the
    /// spelling row that finds them after a restart.
    Rekey(RekeyWrite),
    /// A pool's shedding round on an account: the round's time, which replaces the pool's previous one on the account
    /// so the dwell survives a restart. The marks travel on the pin rows the round writes beside it.
    ShedRound {
        pool_id: String,
        account_id: String,
        incarnation: u64,
        round_at: i64,
    },
    UpsertAffinity(Vec<AffinityRow>),
    UpsertCooldown(CooldownRow),
    UpsertCredits(CreditsRow),
    UpsertOverage(OverageRow),
    UpsertPin(PinRow),
    UpsertWindow(WindowRow),
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Purged {
    /// The removed entries' keys, for the store's delete.
    pub affinity: Vec<(String, String)>,
    /// The same entries whole, with why each left, for the expiry events.
    pub affinity_entries: Vec<PurgedEntry>,
    pub cooldowns: Vec<(String, String)>,
    /// Each dropped pin with the one fact that dropped it: `deleted`, `expired`, or `unserved`.
    pub pins: Vec<(PinKey, &'static str)>,
    /// The slow-down entries dropped, by pool id and account id, whole: an open span ends with its entry.
    pub slow_downs: Vec<((String, String), SlowDownState)>,
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
/// a candidate kept on extra usage with a spent window (0 for every other), priority, and the overage bit.
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
    rate: Arc<Rate>,
    equivalent: Arc<Equivalent>,
    band_segments: Arc<BandSegments>,
}

type Price = dyn Fn(&str, Option<&str>, &str, &str, PinUsage, i64) -> Option<f64> + Send + Sync;
type Measured = dyn Fn(&str) -> bool + Send + Sync;
type Rate = dyn Fn(&str, Option<&str>, &str, &str, &PinBurn, i64) -> Option<f64> + Send + Sync;
type Equivalent = dyn Fn(&str, Option<&str>, &str, PinUsage, i64) -> Option<f64> + Send + Sync;
type BandSegments = dyn Fn(&str, Option<&str>, i64, i64) -> Vec<BandSegment> + Send + Sync;

/// One piece of a span as the band weighs it: `[from, to)` in milliseconds at `weight`, one over the regime's capacity
/// factor inside the band and 1 outside.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct BandSegment {
    pub from: i64,
    pub to: i64,
    pub weight: f64,
}

impl SharePricer {
    /// `price(window, entrypoint, tier, canonical_model, usage, at)`: the share of `window` on `tier` at `at` (the
    /// band weighs the 5-hour window at a completion) under the regime `entrypoint` names, the base one for `None`,
    /// and `None` for a window the tables cannot price for the model on the tier.
    pub fn new(
        price: impl Fn(&str, Option<&str>, &str, &str, PinUsage, i64) -> Option<f64>
            + Send
            + Sync
            + 'static,
    ) -> SharePricer {
        SharePricer {
            price: Arc::new(price),
            measured: Arc::new(|_| true),
            rate: Arc::new(|_, _, _, _, _, _| None),
            equivalent: Arc::new(|_, _, _, _, _| None),
            band_segments: Arc::new(|_, _, from, to| {
                vec![BandSegment {
                    from,
                    to,
                    weight: 1.0,
                }]
            }),
        }
    }

    /// `measured(window)`: whether the tables name `window` rather than price it as the 7-day share once; every
    /// window reads measured until this is set.
    pub fn with_measured(
        self,
        measured: impl Fn(&str) -> bool + Send + Sync + 'static,
    ) -> SharePricer {
        SharePricer {
            measured: Arc::new(measured),
            ..self
        }
    }

    /// `rate(window, entrypoint, tier, canonical_model, burn, at)`: the share of `window` on `tier` a pin burning
    /// `burn` typed tokens a second consumes per second at the band of `at` under the regime `entrypoint` names, the
    /// base one for `None`. A model without a weight of its own prices at its model tier's latest weight, an
    /// estimate, and the rate is `None` for a model of no weighted model tier, a tier the file lacks or a window the
    /// tier has no capacity on. Every pricer prices nothing here until it is set.
    pub fn with_rate(
        self,
        rate: impl Fn(&str, Option<&str>, &str, &str, &PinBurn, i64) -> Option<f64>
            + Send
            + Sync
            + 'static,
    ) -> SharePricer {
        SharePricer {
            rate: Arc::new(rate),
            ..self
        }
    }

    /// `equivalent(window, entrypoint, canonical_model, usage, at)`: the Max 20x equivalent of `window`, the share a
    /// Max 20x account would spend on `usage` under the regime the entrypoint names, at the band of `at`. `None` for
    /// a window the tables cannot price for the model. Every pricer prices nothing here until it is set.
    pub fn with_equivalent(
        self,
        equivalent: impl Fn(&str, Option<&str>, &str, PinUsage, i64) -> Option<f64>
            + Send
            + Sync
            + 'static,
    ) -> SharePricer {
        SharePricer {
            equivalent: Arc::new(equivalent),
            ..self
        }
    }

    /// `band_segments(window, entrypoint, from, to)`: the span cut at the band's edges for the window under the
    /// regime the entrypoint names. One segment at weight 1 until this is set, so a task spawned without the tables
    /// projects at one rate.
    pub fn with_band_segments(
        self,
        segments: impl Fn(&str, Option<&str>, i64, i64) -> Vec<BandSegment> + Send + Sync + 'static,
    ) -> SharePricer {
        SharePricer {
            band_segments: Arc::new(segments),
            ..self
        }
    }

    pub fn rate(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        burn: &PinBurn,
        at: i64,
    ) -> Option<f64> {
        (self.rate)(window, entrypoint, tier, canonical_model, burn, at)
    }

    pub fn price(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        tier: &str,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        (self.price)(window, entrypoint, tier, canonical_model, usage, at)
    }

    pub fn equivalent(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        canonical_model: &str,
        usage: PinUsage,
        at: i64,
    ) -> Option<f64> {
        (self.equivalent)(window, entrypoint, canonical_model, usage, at)
    }

    pub fn measured(&self, window: &str) -> bool {
        (self.measured)(window)
    }

    /// The segments of `[from, to)`, one at weight 1 over the span where the closure answers none, so no reader
    /// indexes an empty list.
    pub fn band_segments(
        &self,
        window: &str,
        entrypoint: Option<&str>,
        from: i64,
        to: i64,
    ) -> Vec<BandSegment> {
        let segments = (self.band_segments)(window, entrypoint, from, to);
        if segments.is_empty() {
            return vec![BandSegment {
                from,
                to,
                weight: 1.0,
            }];
        }
        segments
    }

    /// The band's weight at one instant: a booking's `burn` was priced at it, so its plain rate is the burn over it.
    pub fn band_weight_at(&self, window: &str, entrypoint: Option<&str>, at: i64) -> f64 {
        self.band_segments(window, entrypoint, at, at.saturating_add(1))
            .first()
            .map_or(1.0, |segment| segment.weight)
    }
}

impl Default for SharePricer {
    /// Prices nothing: a task spawned without the ledger's tables predicts from readings alone.
    fn default() -> Self {
        SharePricer::new(|_, _, _, _, _, _| None)
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

/// The rates of a model's prompts longer than `over` tokens.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct LongContextRates {
    pub over: u64,
    pub rates: Rates,
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

/// A price per token by model and token type; `long_context` the second set of the models priced by prompt length,
/// `tiers` the rates of a model the table does not list by the model tier its id names, and `fallback` prices any
/// other model.
#[derive(Clone, Debug, PartialEq)]
pub struct RateTable {
    pub rows: HashMap<String, Rates>,
    pub long_context: HashMap<String, LongContextRates>,
    /// Keyed by model tier name, the key `listing::tier_segment` reads off an id. Empty for a table that prices by
    /// listed model alone.
    pub tiers: HashMap<String, Rates>,
    pub fallback: Rates,
}

impl RateTable {
    /// Every token of every model at 1, so a burn priced through it is a token count.
    pub fn unit() -> RateTable {
        RateTable {
            rows: HashMap::new(),
            long_context: HashMap::new(),
            tiers: HashMap::new(),
            fallback: Rates {
                input: Some(1.0),
                cache_read: Some(1.0),
                write_5m: Some(1.0),
                write_1h: Some(1.0),
                output: Some(1.0),
            },
        }
    }

    /// The row `listing::row_for` finds in `rows` (the exact id before the undated one, as the ledger's `entry`
    /// resolves it), at its `long_context` rates when that same key carries them and `prompt_tokens` is above
    /// their `over`, else at its own. A model `rows` has no key for reads the `tiers` row of the model tier its id
    /// names (`listing::tier_segment`), else `fallback`.
    pub fn rates_for(&self, canonical_model: &str, prompt_tokens: u64) -> Rates {
        // One key wins for both maps, the exact id before the undated one as the ledger's `entry` resolves it, so
        // a dated row without the block never prices at the undated row's long-context rates.
        let key = row_for(canonical_model, |k| {
            self.rows.get_key_value(k).map(|(key, _)| key)
        });
        let Some(key) = key else {
            // The ledger prices an id no file lists through the model tier its id names, so the shed ranks its
            // pin by the same estimate rather than last.
            return tier_segment(canonical_model)
                .and_then(|tier| self.tiers.get(tier))
                .copied()
                .unwrap_or(self.fallback);
        };
        match self.long_context.get(key) {
            Some(long) if prompt_tokens > long.over => long.rates,
            _ => self.rows[key],
        }
    }

    /// The price of one second of a pin's burn at the set its `prompt_tokens` select; `None` when a type the pin
    /// burns has no rate.
    pub fn price_burn(
        &self,
        canonical_model: &str,
        burn: &PinBurn,
        prompt_tokens: u64,
    ) -> Option<f64> {
        let rates = self.rates_for(canonical_model, prompt_tokens);
        Some(
            priced(burn.input, rates.input)?
                + priced(burn.cache_read, rates.cache_read)?
                + priced(burn.write_5m, rates.write_5m)?
                + priced(burn.write_1h, rates.write_1h)?
                + priced(burn.output, rates.output)?,
        )
    }

    /// Each count times its rate at the set `prompt_tokens` select, summed; `None` when a type with a count has no
    /// rate.
    pub fn price(&self, canonical_model: &str, usage: PinUsage, prompt_tokens: u64) -> Option<f64> {
        let rates = self.rates_for(canonical_model, prompt_tokens);
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
    pub fn as_str(self) -> &'static str {
        match self {
            BurnUnit::Usd => "usd",
            BurnUnit::WindowShare => "window_share",
        }
    }

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

    /// The table `rates` prices this unit's copy through: under `WindowShare` the 5-hour table of the regime
    /// `entrypoint` names, the base one for `None`. Dollars cost the same whatever the entrypoint.
    pub fn table<'a>(self, rates: &'a BurnRates, entrypoint: Option<&str>) -> &'a RateTable {
        match self {
            BurnUnit::Usd => &rates.dollars,
            BurnUnit::WindowShare => rates.five_hour.for_entrypoint(entrypoint),
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

/// The realized speed a write folds in, read as 1 (no slowing known) when it is not finite or not positive, since the
/// demand divides by it.
fn full_speed_unless_positive(realized: f64) -> f64 {
    if realized.is_finite() && realized > 0.0 {
        realized
    } else {
        1.0
    }
}

/// Where a dropped or replaced entry's open span ends: the factor's return where it returned before `reset`, the
/// passed reset of the window it slowed for where that reset is what dropped it, else the entry's last read.
fn span_end(entry: &SlowDownState, reset: Option<i64>) -> i64 {
    match (entry.clear_since, reset) {
        (Some(clear), Some(reset)) => clear.min(reset),
        (Some(clear), None) => clear,
        (None, Some(reset)) => reset,
        (None, None) => entry.read_at,
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

/// Past this age a permit or a ticket is a guard whose drop never reached the table: the relay bounds a
/// stream's silence, not its length, so a request older than the longest turn a client runs is one the table
/// gives up waiting on.
pub const IN_FLIGHT_RECLAIM_MS: i64 = 3_600_000;

/// The gaps a cycle averages over. Eight arrivals read a change of pace within a few minutes of 30 s turns, under
/// any burn average, and let one long gap under the span fade from the cycle by an eighth with each later turn.
pub const CYCLE_GAPS: u32 = 8;

/// A move's or a placement's booking on its destination: the rewrite and the burn the request will land there,
/// counted by `predicted` until the usage releases it before its charge, the request ends without usage, or the purge
/// gives up on it.
/// Memory only: a restart drops the map with the in-flight requests it belonged to.
#[derive(Clone, Debug, PartialEq)]
pub struct Reservation {
    pub account_id: String,
    /// The wire attempt whose selection booked it, a fact for the record and the expiry event. The map's key is the
    /// request id alone, so a retry in place or a refresh pass keeps the booking and a failover's selection replaces
    /// it.
    pub attempt: u32,
    /// The pin the selection wrote, `None` for a request that pins nothing and still lands its body.
    pub pin: Option<PinKey>,
    /// The entrypoint `burn` was priced under at `booked_at` (the pin's own for a pin's move, the failed-over row's for
    /// a failover's landing, the request's for a fresh session's median), which a reader that weighs the burn over a
    /// span reads its band under. `rewrite` is a one-time share priced under the request's regime, which no reader
    /// weighs over a span. `None` is the base regime.
    pub entrypoint: Option<String>,
    pub booked_at: i64,
    /// The rewrite's share per plan window, what `predicted` adds.
    pub rewrite: BTreeMap<String, f64>,
    /// The pin's burn as share per second per plan window, what `burn_shares` adds.
    pub burn: BTreeMap<String, f64>,
    pub expires_at: i64,
}

/// An account's slow-down on one pool's pins, as the last pinned selection on the account read it, and the span the
/// factor has read under 1 for.
#[derive(Clone, Debug, PartialEq)]
pub struct SlowDownState {
    /// The burn the window can still carry to its reset over the demand, clamped to [0, 1].
    pub factor: f64,
    /// The realized speed, a time-weighted average over the pool's burn average, which the demand divides the pins'
    /// burn by: the burn average lags a change of pace by the same constant.
    pub speed: f64,
    pub read_at: i64,
    /// When the stretch first read over the threshold, cleared once it reads at or under it.
    pub over_since: Option<i64>,
    /// When the factor first read under 1 since the span last closed. A return to 1 keeps it through the
    /// `SLOW_DOWN_WAIT_MS` wait, so a dip back under 1 inside the wait continues the span.
    pub slowed_since: Option<i64>,
    /// When the factor last returned to 1 inside the span. A dip back under 1 clears it, and the span's close keeps
    /// it as the instant the factor returned.
    pub clear_since: Option<i64>,
    /// The lowest factor read since `slowed_since`, 1 outside a span.
    pub factor_min: f64,
    /// The new holds answered since `slowed_since`, each from its arrival to its deadline, summed, 0 outside a span. A
    /// request rejoining a standing deadline adds nothing, since its wait is the hold already counted.
    pub held_ms: i64,
    /// The read's stretch, kept beside its factor so the pages read the figures the holds apply.
    pub stretch_ms: i64,
    /// The threshold the read compared the stretch to, the pool's `slow_down_stretch_seconds` at the read.
    pub threshold_ms: i64,
    /// The 5-hour reading's reset at the read, `None` where the reading named none or its reset had passed: the
    /// window the entry was read in, which the stale test compares `read_at` against once the reset has passed.
    pub reset: Option<i64>,
    /// When the read's factor first read under 1 since it last read 1, cleared at a read whose factor reads 1: the
    /// ramp's stamp, from which the factor the holds realize is pulled from 1 to the read's over one burn average.
    /// Unlike `slowed_since` it clears at once, so a burst that ends and comes back ramps again from nothing, and
    /// unlike `over_since` it stamps the stretch's zero crossing, not the threshold's.
    pub ramp_since: Option<i64>,
}

/// One edge of an account's slow-down span, which the task announces as an event.
#[derive(Clone, Debug, PartialEq)]
pub enum SlowDownEdge {
    /// The factor read under 1 for the first time since it last read 1, with the figures of that read: the demand in
    /// share of the 5-hour window per hour.
    Started {
        factor: f64,
        stretch_ms: i64,
        threshold_ms: i64,
        demand: f64,
        reset: Option<i64>,
    },
    /// The span closed, its length running from `since` to the instant the factor returned to 1, to the passed reset
    /// of the window it slowed for where the reset cleared the entry, or to the entry's last read where the purge
    /// dropped the entry before either: the wait that closes a span is not slowed time.
    Ended {
        since: i64,
        duration_ms: i64,
        factor_min: f64,
        held_ms: i64,
    },
}

/// A pin's pace: the cycle a slow-down hold is sized from and the hold standing on it.
#[derive(Clone, Debug, PartialEq)]
pub struct PinPace {
    /// The last dispatch of a request of a held class: its arrival, or the end of the hold it waited through, so a
    /// wait never counts as the pin's own gap.
    pub dispatched_at: i64,
    /// The count-weighted mean of the gaps between a dispatch and the next arrival: the first gap whole, then each gap
    /// under the pool's burn average moves it by one over `min(gaps + 1, CYCLE_GAPS)`. `None` until a gap under the
    /// pool's burn average has been measured.
    pub cycle_ms: Option<f64>,
    /// The gaps folded so far, at most `CYCLE_GAPS`: the next fold's share is one over this count plus one, held at
    /// one over `CYCLE_GAPS` once the count is there.
    pub gaps: u32,
    /// The end of the slow-down hold standing on the pin.
    pub deadline: Option<i64>,
}

/// The request a moved pin's next requests wait for the head of, the wire attempt whose leg carries it, and the
/// instant its selection dispatched it: the upstream makes a cache entry readable only once the response that
/// writes it has begun, so a request out before that head would write the prefix again. Memory only, as the pace
/// is: the next landing rebuilds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HeadMark {
    pub request_id: String,
    /// The selection's `attempt`, the leg's number: a failover lands the same request again under the next one.
    pub attempt: u32,
    pub at: i64,
}

/// The `window_ceiling` a pool is created with.
pub const DEFAULT_WINDOW_CEILING: f64 = 0.95;

/// The utilization of `window` on `pool` above which no move or placement lands and a round fires on the window. The
/// row holds one fraction for every plan window, read through this one function, so a ceiling per window is one
/// column and one arm here.
pub fn ceiling(pool: &PoolRow, _window: &str) -> f64 {
    pool.window_ceiling
}

/// The share of an account's burn one shedding round removes at most.
pub fn shed_cap(pool: &PoolRow) -> f64 {
    pool.shedding.shed_cap
}

/// The time constant a pin's burn average decays with, in milliseconds.
pub fn burn_average_ms(pool: &PoolRow) -> i64 {
    seconds_ms(pool.shedding.burn_average_seconds)
}

/// `burn_average_ms` of the default preset, for a key whose pool left the registry.
pub(crate) fn default_burn_average_ms() -> i64 {
    seconds_ms(phoebis_store::Shedding::default().burn_average_seconds)
}

/// The pause between two shedding rounds on one account, and how long a shed pin keeps off the account that shed it,
/// in milliseconds.
pub fn shed_dwell_ms(pool: &PoolRow) -> i64 {
    seconds_ms(pool.shedding.shed_dwell_seconds)
}

/// Whether the pool runs shedding rounds. Off, the fit gate, the reservations, the projection and the move off a spent
/// account still run, since the switch turns the round off and nothing else.
pub fn load_shedding(pool: &PoolRow) -> bool {
    pool.shedding.load_shedding
}

/// Whether the pool slows its pins' requests on the 5-hour window.
pub fn slow_down(pool: &PoolRow) -> bool {
    pool.slow_down.slow_down
}

/// The time the slow-down may add to a session before the round takes over, in milliseconds.
pub fn slow_down_stretch_ms(pool: &PoolRow) -> i64 {
    seconds_ms(u64::from(pool.slow_down.slow_down_stretch_seconds))
}

/// The longest one request waits for the slow-down, in milliseconds.
pub fn slow_down_hold_ms(pool: &PoolRow) -> i64 {
    seconds_ms(u64::from(pool.slow_down.slow_down_hold_seconds))
}

/// How long a warm pin stays on an api_key member before the return rule moves it under `always`, in milliseconds.
pub fn return_to_plan_dwell_ms(pool: &PoolRow) -> i64 {
    seconds_ms(u64::from(pool.failover.return_to_plan_dwell_seconds))
}

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

    /// Whether the ticket `id` still counts on `account_id`: false once its guard dropped or the hour's reclaim
    /// took it.
    pub fn holds(&self, account_id: &str, id: u64) -> bool {
        self.0
            .table()
            .accounts
            .get(account_id)
            .is_some_and(|slots| slots.contains_key(&id))
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

impl AccountTicket {
    /// The ticket's id in the in-flight table, which a funding retry's claim names.
    pub fn id(&self) -> u64 {
        self.id
    }
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

/// Whether the pin's mark stands: `marked_at` set and `marked_on` its current account. A row carrying a mark made on
/// another account was moved by a write that kept the members, and that mark no longer stands.
pub(crate) fn mark_holds(pin: &PinRow) -> bool {
    pin.marked_at.is_some() && pin.marked_on.as_deref() == Some(pin.account_id.as_str())
}

/// The loaded rounds by pool and account: each row's time, what the dwell between rounds reads.
fn rounds_of(load: &RoutingLoad) -> HashMap<(String, String), i64> {
    load.shed_rounds
        .iter()
        .map(|row| ((row.pool_id.clone(), row.account_id.clone()), row.round_at))
        .collect()
}

/// A `funding_required` account's retry: the instant one request may try it again, the incarnation the mark landed
/// on, and the account ticket of the attempt that holds the one slot. Memory only: a restart reads the mark with no
/// entry, and the first request after it retries at once.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FundingRetry {
    pub retry_at: i64,
    pub incarnation: u64,
    /// The in-flight id of the claiming attempt's `AccountTicket`. The claim lasts while the ticket is in the
    /// table, so a slow upstream head never frees it early.
    pub claimed_by: Option<u64>,
    /// When the entry was made: the mark's landing, or the claim that found none after a restart. A retry's success
    /// that went out before it says nothing of the balance the mark found.
    pub stamped_at: i64,
    /// The account row's revision once the writer acknowledged the mark's write, `None` until then. A load that read
    /// the row below it read the registry before the mark landed, so it drops nothing. A mark the fences refused and a
    /// claim, which writes nothing, read 0.
    pub mark_revision: Option<u64>,
}

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
    /// Each `funding_required` account's retry by account id. Memory only: the store keeps no instant for a mark,
    /// so a restart starts with none.
    funding_retries: HashMap<String, FundingRetry>,
    /// The request each moved pin's next requests wait for the head of, by the pin's key. Memory only, as the paces
    /// are, and it leaves with the row in `take_pin`, so a restart holds none.
    head_marks: HashMap<PinKey, HeadMark>,
    in_flight: InFlight,
    /// The ledger's last-answered spend per pool member and budgeted period, by `(pool_id, account_id, period)`;
    /// memory only.
    member_spend: HashMap<(String, String, BudgetPeriod), MemberSpend>,
    next_epoch: u64,
    overage: HashMap<String, OverageEvidence>,
    pins: HashMap<PinKey, PinRow>,
    /// Each pin's pace by its key, which outlives `take_pin`: a move and a failover's landing write their fresh row
    /// under the same key, and the session's pace does not change with its account. Memory only, so the first request
    /// of a pin after a restart reads none.
    pin_paces: HashMap<PinKey, PinPace>,
    /// When each window of the projection test first read over the ceiling at a pinned selection, keyed by pool id,
    /// account id and window. Memory only: a restart starts from none.
    projection_over: HashMap<(String, String, String), i64>,
    /// The ledger's tables, which this crate cannot name, handed in by the task; unit tables until then, so a
    /// burn read through them is a token count.
    rates: BurnRates,
    /// The pins some selection or completion renewed since their placement. A client error on the request that
    /// created a pin releases the pin only while its key is absent here: a renewal means the pin serves another
    /// turn, however close in time. Memory only, since no request outlives a restart to report on a loaded pin.
    renewed: HashSet<PinKey>,
    /// The moves and placements in flight, by request id: what `predicted` adds beside the charges until the usage
    /// or the end releases each. Memory only, with the in-flight requests it belongs to.
    reservations: HashMap<String, Reservation>,
    /// A folded spelling to the dated id it resolved to, so every lookup below can key a model's rows on the
    /// dated id once it is known, whichever spelling the caller names.
    resolutions: HashMap<String, String>,
    round_robin: HashMap<(String, Tier, String), usize>,
    samples: HashMap<String, UtilizationSample>,
    /// The ledger's share pricer, handed in by the task as `rates` is. Until then one that prices nothing, so a
    /// selection books no reservation and a burn reads empty.
    share: SharePricer,
    /// When the latest shedding round ran on each account, by the id of the pool that ran it and the account id: the
    /// stamp the dwell between rounds reads. The marks live on the pin rows.
    shed_rounds: HashMap<(String, String), i64>,
    /// The span edges the writes and the purge recorded and the task has yet to announce, by pool id and account id.
    slow_down_edges: Vec<(String, String, SlowDownEdge)>,
    /// Each account's slow-down by the id of the pool whose pins it reads and the account id, as the last pinned
    /// selection on the account wrote it. Memory only, as the reservations are: a restart starts every account
    /// unslowed.
    slow_downs: HashMap<(String, String), SlowDownState>,
}

impl RoutingState {
    pub fn from_load(load: RoutingLoad) -> RoutingState {
        let next_epoch = load.pins.iter().map(|p| p.epoch).max().unwrap_or(0) + 1;
        let shed_rounds = rounds_of(&load);
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
                    source: WindowSource::Stored,
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
            funding_retries: HashMap::new(),
            head_marks: HashMap::new(),
            in_flight: InFlight::default(),
            member_spend: HashMap::new(),
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
            pin_paces: HashMap::new(),
            projection_over: HashMap::new(),
            rates: BurnRates::default(),
            renewed: HashSet::new(),
            reservations: HashMap::new(),
            resolutions: load
                .resolutions
                .into_iter()
                .map(|r| (r.spelling, r.dated_id))
                .collect(),
            round_robin: HashMap::new(),
            samples,
            share: SharePricer::default(),
            shed_rounds,
            slow_down_edges: Vec::new(),
            slow_downs: HashMap::new(),
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

    /// A fresh pin under a key another pin held (a release that lands the session elsewhere) takes the old one off
    /// its account with its mark and starts unrenewed. A renewal keeps the epoch and the renewal, and the row it
    /// writes carries the mark as the caller read it. A fresh pin on another account than the row it replaces clears
    /// the slow-down deadline standing on the key's pace and keeps the cycle and the last dispatch: the hold was sized
    /// for the source's window, and the new account's own factor decides the next one. A fresh pin with no row to
    /// replace counts as such a landing, since a move and a failover take the row off the source before they write.
    pub(crate) fn put_pin(&mut self, mut pin: PinRow) {
        pin.model = self.canonical(&pin.model).to_string();
        let key = pin.key();
        let epoch = pin.epoch;
        let account_id = pin.account_id.clone();
        let landed = match self.pins.insert(key.clone(), pin) {
            Some(replaced) if replaced.epoch == epoch => false,
            replaced => {
                self.renewed.remove(&key);
                replaced.is_none_or(|replaced| replaced.account_id != account_id)
            }
        };
        if landed {
            if let Some(pace) = self.pin_paces.get_mut(&key) {
                pace.deadline = None;
            }
        }
    }

    /// The pin's fast usage, typed tokens per second decayed from the pin's `burn_at` as `PinRow::burn` is, and
    /// zero for a pin never renewed fast.
    pub fn fast_burn(&self, key: &PinKey) -> PinBurn {
        self.pin(key).map(|p| p.burn_fast).unwrap_or_default()
    }

    /// Sets the held pin's fast usage in the state alone, for a test that prices a round over fast usage. A renewal
    /// folds its fast usage into the row the task both puts here and writes, so the stored row never lags the state.
    pub fn set_fast_burn(&mut self, key: &PinKey, burn: PinBurn) {
        let key = PinKey {
            model: self.canonical(&key.model).to_string(),
            ..key.clone()
        };
        if let Some(pin) = self.pins.get_mut(&key) {
            pin.burn_fast = burn;
        }
    }

    /// Every pin with fast usage, by key. The fast-mode tests probe the copy through it and no production path reads
    /// it, so it is compiled under `test` and the `debug-hooks` feature alone.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn fast_burns(&self) -> HashMap<PinKey, PinBurn> {
        self.pins
            .iter()
            .filter(|(_, p)| p.burn_fast != PinBurn::default())
            .map(|(key, p)| (key.clone(), p.burn_fast))
            .collect()
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

    /// The one way a pin leaves the map, by a move, a fail-over, an expiry, a release or the purge. The row leaves
    /// with its mark, which the caller reads off the answer to say what cleared it.
    fn take_pin(&mut self, key: &PinKey) -> Option<PinRow> {
        let pin = self.pins.remove(key)?;
        self.renewed.remove(key);
        self.head_marks.remove(key);
        Some(pin)
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
        let index = self.round_robin_turn(pool_id, tier, cohort);
        if advance {
            self.round_robin
                .insert((pool_id.to_string(), tier, cohort.join(",")), index + 1);
        }
        index
    }

    /// The index `round_robin_next` answers for the cohort now, with the turn left where it is: what a round's plan
    /// reads, so the destination it names is the one the pin's own placement draws when nothing changes before it.
    pub(crate) fn round_robin_turn(&self, pool_id: &str, tier: Tier, cohort: &[&str]) -> usize {
        let key = (pool_id.to_string(), tier, cohort.join(","));
        self.round_robin
            .get(&key)
            .map_or(0, |slot| slot % cohort.len())
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

    /// Stamps the account's retry `interval_ms` past `now` under the mark's `incarnation`, and frees the slot: the
    /// attempt that held it ended in this mark.
    pub fn funding_marked(
        &mut self,
        account_id: &str,
        incarnation: u64,
        now: i64,
        interval_ms: i64,
    ) {
        self.funding_retries.insert(
            account_id.to_string(),
            FundingRetry {
                retry_at: now.saturating_add(interval_ms),
                incarnation,
                claimed_by: None,
                stamped_at: now,
                mark_revision: None,
            },
        );
    }

    /// The writer finished the mark's write for `account_id` under `incarnation`: landed at `revision`, or refused by
    /// its fences.
    pub fn funding_written(&mut self, account_id: &str, incarnation: u64, revision: Option<u64>) {
        if let Some(retry) = self
            .funding_retries
            .get_mut(account_id)
            .filter(|retry| retry.incarnation == incarnation && retry.mark_revision.is_none())
        {
            retry.mark_revision = Some(revision.unwrap_or(0));
        }
    }

    pub fn funding_cleared(&mut self, account_id: &str) {
        self.funding_retries.remove(account_id);
    }

    /// Whether the account's entry was stamped after `at`, the instant an attempt went out.
    pub fn funding_stamped_after(&self, account_id: &str, at: i64) -> bool {
        self.funding_retries
            .get(account_id)
            .is_some_and(|retry| retry.stamped_at > at)
    }

    /// Whether a `funding_required` account may take a request at `now`: at once with no entry, as after a restart,
    /// else once its retry is due and no live attempt holds the slot.
    pub fn funding_retry_admits(&self, account_id: &str, now: i64) -> bool {
        let Some(retry) = self.funding_retries.get(account_id) else {
            return true;
        };
        now >= retry.retry_at
            && retry
                .claimed_by
                .is_none_or(|id| !self.in_flight.holds(account_id, id))
    }

    /// Hands the slot to the attempt the account ticket `ticket` counts. An account with no entry for
    /// `incarnation`, as after a restart, gets one due at `now`, so the next request finds the slot taken.
    pub fn claim_funding_retry(
        &mut self,
        account_id: &str,
        incarnation: u64,
        ticket: u64,
        now: i64,
    ) {
        match self.funding_retries.get_mut(account_id) {
            Some(retry) if retry.incarnation == incarnation => retry.claimed_by = Some(ticket),
            _ => {
                self.funding_retries.insert(
                    account_id.to_string(),
                    FundingRetry {
                        retry_at: now,
                        incarnation,
                        claimed_by: Some(ticket),
                        stamped_at: now,
                        mark_revision: Some(0),
                    },
                );
            }
        }
    }

    /// Keeps an entry only while `snapshot` reads its account `FundingRequired` under the entry's incarnation, so a
    /// Clear, a re-pairing or a deletion that reached the registry drops it. Under the same incarnation an entry whose
    /// mark the writer has not acknowledged, or one the snapshot reads at a row revision below the mark's, stays
    /// whatever the snapshot reads, since that load read the registry before the mark's write landed.
    pub fn reconcile_funding_retries(&mut self, snapshot: &RegistrySnapshot) {
        self.funding_retries.retain(|account_id, retry| {
            snapshot.account(account_id).is_some_and(|account| {
                account.incarnation == retry.incarnation
                    && (account.state == AccountState::FundingRequired
                        || retry
                            .mark_revision
                            .is_none_or(|written| account.revision < written))
            })
        });
    }

    /// Each entry's `(account_id, retry_at)`, sorted by account, for the account view.
    pub fn funding_retries(&self) -> Vec<(String, i64)> {
        let mut v: Vec<_> = self
            .funding_retries
            .iter()
            .map(|(id, retry)| (id.clone(), retry.retry_at))
            .collect();
        v.sort();
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
    /// names one. A reading the router's own figure moved names `Predicted` as its source. One nothing moved keeps
    /// its own. `None` while the account holds no reading of the window.
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
        // A booking counts whatever the reading's age: no reading carries a charge that has not happened.
        let reserved = self.reserved(account_id, window);
        match reading.reset {
            Some(reset) if reset <= now => match window {
                // The weekly windows reset at the same weekday time every week, so the reading's reset names every
                // later one; the 5-hour window opens with the next request and names its reset in the next reading.
                w if w.starts_with(SEVEN_DAY) => {
                    let next = reset + ((now - reset) / WEEK_MS + 1) * WEEK_MS;
                    Some(WindowReading {
                        utilization: since(next - WEEK_MS) + reserved,
                        reset: Some(next),
                        observed_at: reading.observed_at,
                        source: WindowSource::Predicted,
                    })
                }
                _ => Some(WindowReading {
                    utilization: since(reset) + reserved,
                    reset: None,
                    observed_at: reading.observed_at,
                    source: WindowSource::Predicted,
                }),
            },
            _ => {
                let charged = since(reading.observed_at) + reserved;
                Some(WindowReading {
                    utilization: reading.utilization + charged,
                    source: if charged != 0.0 {
                        WindowSource::Predicted
                    } else {
                        reading.source
                    },
                    ..reading
                })
            }
        }
    }

    /// When the account's `window` started, read from the stored reading and never from `predicted`, which drops a
    /// passed 5-hour reset: the reset less the window's length while the reset lies after `now`, the passed reset
    /// itself for a 5-hour reading at or before `now` (the window opened there with the account's next request, and the
    /// next reading names the new reset), a passed weekly reset rolled forward by whole weeks as `predicted` rolls it,
    /// less `WEEK_MS`, and `None` for a reading that names no reset or for no reading at all. The stamp's start and the
    /// hold on the 5-hour round read it. The start lies at or after `now` for a reset more than a window's length
    /// ahead, which the hold reads as no age.
    pub fn window_started(&self, account_id: &str, window: &str, now: i64) -> Option<i64> {
        let reset = self.samples.get(account_id)?.reading(window)?.reset?;
        Some(if window.starts_with(SEVEN_DAY) {
            // A weekly reset names every later one, so a passed reset rolls forward as `predicted` rolls it.
            let next = if reset > now {
                reset
            } else {
                reset + ((now - reset) / WEEK_MS + 1) * WEEK_MS
            };
            next - WEEK_MS
        } else if reset > now {
            reset - crate::select::FIVE_HOUR_MS
        } else {
            reset
        })
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

    /// Set by the task at spawn from its config, and by a test that prices a selection through its own closures.
    pub fn set_share(&mut self, share: SharePricer) {
        self.share = share;
    }

    pub fn share(&self) -> &SharePricer {
        &self.share
    }

    /// Books under the request's id, replacing a booking the request already holds (a failover's new selection).
    pub fn book_reservation(&mut self, request_id: &str, reservation: Reservation) {
        self.reservations
            .insert(request_id.to_string(), reservation);
    }

    pub fn reservation(&self, request_id: &str) -> Option<&Reservation> {
        self.reservations.get(request_id)
    }

    /// The `Usage` and `Ended` paths: the booking leaves and nothing is charged, `None` when the request holds none.
    /// The usage arm calls it before `charge`, which it calls on every usage as today.
    pub fn release_reservation(&mut self, request_id: &str) -> Option<Reservation> {
        self.reservations.remove(request_id)
    }

    /// Every booking past its `expires_at`, removed, for the purge's events.
    pub fn expire_reservations(&mut self, now: i64) -> Vec<(String, Reservation)> {
        let mut expired: Vec<String> = self
            .reservations
            .iter()
            .filter(|(_, reservation)| reservation.expires_at <= now)
            .map(|(request_id, _)| request_id.clone())
            .collect();
        // One order for the purge's events, whatever the map's.
        expired.sort();
        expired
            .into_iter()
            .filter_map(|request_id| {
                self.reservations
                    .remove(&request_id)
                    .map(|reservation| (request_id, reservation))
            })
            .collect()
    }

    /// The live reservations' rewrite on the account and window summed, what `predicted` adds.
    pub fn reserved(&self, account_id: &str, window: &str) -> f64 {
        self.reservations
            .values()
            .filter(|reservation| reservation.account_id == account_id)
            .filter_map(|reservation| reservation.rewrite.get(window))
            .fold(0.0, |sum, share| sum + share)
    }

    pub fn reservations_on(&self, account_id: &str) -> Vec<&Reservation> {
        let mut on: Vec<(&String, &Reservation)> = self
            .reservations
            .iter()
            .filter(|(_, reservation)| reservation.account_id == account_id)
            .collect();
        on.sort_by(|a, b| a.0.cmp(b.0));
        on.into_iter().map(|(_, reservation)| reservation).collect()
    }

    /// The account's slow-down entry on the pool as written, whatever the pool's switch or the window's reset reads
    /// now.
    pub fn slow_down_state(&self, pool_id: &str, account_id: &str) -> Option<&SlowDownState> {
        self.slow_downs
            .get(&(pool_id.to_string(), account_id.to_string()))
    }

    /// The read's write, once per pinned selection on the account: the factor from the read, the speed folded toward
    /// `realized` (`speed x w + realized x (1 - w)` with `w = exp(-(now - read_at) / average_ms)`, the first write
    /// taking `realized` whole), `over_since` stamped while the read's stretch is over its threshold and cleared at or
    /// under it, `ramp_since` stamped while the factor reads under 1 and cleared at 1, and the span the factor reads
    /// under 1 for opened and tracked. A read of 1 stamps `clear_since` and leaves the span open, so a dip back under 1
    /// inside `SLOW_DOWN_WAIT_MS` continues it and `take_slow_down_edges` closes it once the factor has read 1 for the
    /// wait. A span that opens records its start from this read, after the end of a span whose wait ran out before any
    /// drain closed it. An entry read before its window's passed reset is replaced: its open span ends by that reset,
    /// and this read writes the entry afresh as a first write does. Answers the entry as written, the read's stretch
    /// and threshold beside its factor.
    pub fn write_slow_down(
        &mut self,
        pool_id: &str,
        account_id: &str,
        read: &SlowDownRead,
        realized: f64,
        now: i64,
        average_ms: i64,
    ) -> &SlowDownState {
        debug_assert!(
            realized.is_finite() && realized > 0.0,
            "a realized speed is positive and finite: {realized}"
        );
        let realized = full_speed_unless_positive(realized);
        let over = read.stretch_ms > read.threshold_ms;
        let slowed = read.factor < 1.0;
        let key = (pool_id.to_string(), account_id.to_string());
        // An entry read before its window's passed reset is the old window's: its span ends by that reset, and no speed
        // or stamp of it carries into the window this read opens.
        if let Some(reset) = self
            .slow_downs
            .get(&key)
            .and_then(|entry| self.stale_reset(account_id, entry, now))
        {
            let stale = self
                .slow_downs
                .remove(&key)
                .expect("the entry was just read");
            if let Some(since) = stale.slowed_since {
                self.slow_down_edges.push((
                    pool_id.to_string(),
                    account_id.to_string(),
                    SlowDownEdge::Ended {
                        since,
                        duration_ms: span_end(&stale, Some(reset)) - since,
                        factor_min: stale.factor_min,
                        held_ms: stale.held_ms,
                    },
                ));
            }
        }
        let mut opened = !self.slow_downs.contains_key(&key) && slowed;
        let mut ended = None;
        self.slow_downs
            .entry(key)
            .and_modify(|entry| {
                let w = (-((now - entry.read_at).max(0) as f64) / average_ms.max(1) as f64).exp();
                entry.speed = entry.speed * w + realized * (1.0 - w);
                entry.factor = read.factor;
                entry.stretch_ms = read.stretch_ms;
                entry.threshold_ms = read.threshold_ms;
                entry.read_at = now;
                entry.reset = read.reset;
                entry.over_since = over.then(|| entry.over_since.unwrap_or(now));
                entry.ramp_since = slowed.then(|| entry.ramp_since.unwrap_or(now));
                // A span whose factor has read 1 for the wait ended at the return, though no drain has closed it yet: a
                // selection can land before the purge tick that would, and the dip it reads opens a span of its own.
                if let (true, Some(since), Some(clear)) =
                    (slowed, entry.slowed_since, entry.clear_since)
                {
                    if now - clear >= SLOW_DOWN_WAIT_MS {
                        ended = Some(SlowDownEdge::Ended {
                            since,
                            duration_ms: clear - since,
                            factor_min: entry.factor_min,
                            held_ms: entry.held_ms,
                        });
                        entry.slowed_since = None;
                    }
                }
                match (slowed, entry.slowed_since) {
                    (true, Some(_)) => {
                        entry.factor_min = entry.factor_min.min(read.factor);
                        entry.clear_since = None;
                    }
                    (true, None) => {
                        entry.slowed_since = Some(now);
                        entry.clear_since = None;
                        entry.factor_min = read.factor;
                        entry.held_ms = 0;
                        opened = true;
                    }
                    (false, Some(_)) => {
                        entry.clear_since.get_or_insert(now);
                    }
                    (false, None) => {}
                }
            })
            .or_insert_with(|| SlowDownState {
                factor: read.factor,
                speed: realized,
                read_at: now,
                reset: read.reset,
                over_since: over.then_some(now),
                ramp_since: slowed.then_some(now),
                slowed_since: slowed.then_some(now),
                clear_since: None,
                factor_min: read.factor.min(1.0),
                held_ms: 0,
                stretch_ms: read.stretch_ms,
                threshold_ms: read.threshold_ms,
            });
        if let Some(edge) = ended {
            self.slow_down_edges
                .push((pool_id.to_string(), account_id.to_string(), edge));
        }
        if opened {
            self.slow_down_edges.push((
                pool_id.to_string(),
                account_id.to_string(),
                SlowDownEdge::Started {
                    factor: read.factor,
                    stretch_ms: read.stretch_ms,
                    threshold_ms: read.threshold_ms,
                    demand: read.demand * 3_600.0,
                    reset: read.reset,
                },
            ));
        }
        self.slow_downs
            .get(&(pool_id.to_string(), account_id.to_string()))
            .expect("the entry was just written")
    }

    /// The span edges to announce at `now`: those the writes and the purge recorded, in the order they were recorded,
    /// then one end per span whose factor has read 1 for `SLOW_DOWN_WAIT_MS`, its length from `slowed_since` to
    /// `clear_since`. A closed span leaves the entry unslowed with its figures reset and `clear_since` the instant the
    /// factor last returned to 1. Every entry is read, so an account that returned to 1 and went quiet ends its span at
    /// the next purge tick without a selection.
    pub fn take_slow_down_edges(&mut self, now: i64) -> Vec<(String, String, SlowDownEdge)> {
        let mut edges = std::mem::take(&mut self.slow_down_edges);
        let mut closed: Vec<(String, String, SlowDownEdge)> = self
            .slow_downs
            .iter_mut()
            .filter_map(|((pool_id, account_id), entry)| {
                let since = entry.slowed_since?;
                let clear = entry
                    .clear_since
                    .filter(|clear| now - clear >= SLOW_DOWN_WAIT_MS)?;
                let edge = SlowDownEdge::Ended {
                    since,
                    duration_ms: clear - since,
                    factor_min: entry.factor_min,
                    held_ms: entry.held_ms,
                };
                entry.slowed_since = None;
                entry.factor_min = 1.0;
                entry.held_ms = 0;
                Some((pool_id.clone(), account_id.clone(), edge))
            })
            .collect();
        // The map's order is no order, and the events land in the ledger in the order they are emitted.
        closed.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
        edges.extend(closed);
        edges
    }

    /// Adds a slow-down hold's length to the entry's `held_ms`, the span's held time. Nothing while no span is open:
    /// the time belongs to a span that stands, and a request rejoining a standing deadline is not passed here, so two
    /// requests of one pin waiting to one instant count once.
    pub fn add_slow_down_held(&mut self, pool_id: &str, account_id: &str, held_ms: i64) {
        if let Some(entry) = self
            .slow_downs
            .get_mut(&(pool_id.to_string(), account_id.to_string()))
            .filter(|entry| entry.slowed_since.is_some())
        {
            entry.held_ms = entry.held_ms.saturating_add(held_ms);
        }
    }

    /// The passed reset the entry was read before, where there is one: the later of the stored 5-hour reading's
    /// reset and the entry's own among those at or before `now`. A head's whole seconds and a poll's milliseconds
    /// spell one reset two ways under a second apart, and whichever spelling has passed answers, since an entry
    /// read before either was read in the old window. A reading that names no reset, as a poll of an idle window
    /// does, and no reading at all add no candidate, so the entry's own reset answers alone. `None` while no
    /// candidate has passed.
    fn stale_reset(&self, account_id: &str, entry: &SlowDownState, now: i64) -> Option<i64> {
        let reading = self
            .samples
            .get(account_id)
            .and_then(|sample| sample.reading(FIVE_HOUR))
            .and_then(|reading| reading.reset);
        let passed = [reading, entry.reset]
            .into_iter()
            .flatten()
            .filter(|reset| *reset <= now)
            .max()?;
        (entry.read_at < passed).then_some(passed)
    }

    /// Whether the entry was read before the passed reset of the window it was read in, so it is the old window's.
    pub fn slow_down_stale(&self, account_id: &str, entry: &SlowDownState, now: i64) -> bool {
        self.stale_reset(account_id, entry, now).is_some()
    }

    /// The entry in force for the account on the pool at `now`: `None` at once for a pool whose switch is off, whatever
    /// lingers until the purge tick drops it, `None` for an entry read before its window's passed reset, so every
    /// reader sees the window's reset clear the account at once, and `None` where no entry stands, an api_key account,
    /// an untiered account or one with no 5-hour reading (which writes no entry) and an account no pinned selection has
    /// read since the last purge or restart. The test reads the reset and not the window's start: `window_started`
    /// opens the new window with the account's next request, so after an idle gap past the reset the start is that
    /// request's upstream receipt, which falls just after the `read_at` of the selection that sent it, and a test on
    /// the start would clear that entry again at the next read.
    pub fn slow_down_in_force(
        &self,
        pool: &PoolRow,
        account_id: &str,
        now: i64,
    ) -> Option<&SlowDownState> {
        slow_down(pool)
            .then(|| self.slow_down_state(&pool.id, account_id))
            .flatten()
            .filter(|entry| !self.slow_down_stale(account_id, entry, now))
    }

    /// The speed the demand divides the account's pins' burn by on `pool` at `now`: the realized speed of the entry in
    /// force, and 1 where none is, as for a stale entry.
    pub(crate) fn slow_down_speed(&self, pool: &PoolRow, account_id: &str, now: i64) -> f64 {
        self.slow_down_in_force(pool, account_id, now)
            .map_or(1.0, |entry| entry.speed)
    }

    /// The pace under `key` as the pin row spells it, with no resolution applied.
    pub fn pin_pace(&self, key: &PinKey) -> Option<&PinPace> {
        self.pin_paces.get(key)
    }

    /// Folds an arrival into the pin's cycle, the mean of its last gaps: the gap since the last dispatch moves the
    /// cycle by `1 / min(gaps + 1, CYCLE_GAPS)` toward itself, so the first gap is taken whole, the first gaps read
    /// their plain mean and every later one moves it by an eighth, and the cycle tracks a change of pace within eight
    /// arrivals. A gap of `average_ms` or longer is a pause that folds nothing and leaves the cycle and its count
    /// standing: a long think, a main agent waiting on its subagents or an idle session says nothing of the pace the
    /// session runs at, and a hold sized from such a gap would hold the resumed session at the cap while the
    /// water-filling counts it almost unslowed. The arrival still becomes the last dispatch, so the next gap is
    /// measured from it. An arrival at or before the last dispatch folds nothing: a re-selection of the same request
    /// (after a failover or a refused hold) carries the arrival already folded, whose gap of zero would plant a cycle
    /// of zero, a request queued behind a later one has no gap to give, and an arrival before a standing deadline,
    /// which a hold made the last dispatch, rejoins the deadline. The pin's first arrival starts its pace with no gap.
    pub fn fold_pace(&mut self, key: &PinKey, received_at: i64, average_ms: i64) {
        let Some(pace) = self.pin_paces.get_mut(key) else {
            self.pin_paces.insert(
                key.clone(),
                PinPace {
                    dispatched_at: received_at,
                    cycle_ms: None,
                    gaps: 0,
                    deadline: None,
                },
            );
            return;
        };
        if received_at <= pace.dispatched_at {
            return;
        }
        let gap = received_at - pace.dispatched_at;
        pace.dispatched_at = received_at;
        if gap >= average_ms {
            return;
        }
        let gap = gap as f64;
        let n = f64::from(pace.gaps.min(CYCLE_GAPS - 1) + 1);
        pace.cycle_ms = Some(match pace.cycle_ms {
            None => gap,
            Some(cycle) => cycle + (gap - cycle) / n,
        });
        pace.gaps = (pace.gaps + 1).min(CYCLE_GAPS);
    }

    /// Ends the pace under `key` with its pin's session: a fresh placement under the key starts with none.
    pub(crate) fn drop_pace(&mut self, key: &PinKey) {
        self.pin_paces.remove(key);
    }

    /// A hold answered for a request of the pin under `key` dispatches the request at its end, so the wait never lands
    /// in the pin's next gap. A later dispatch already standing stays. Nothing for a key with no pace.
    pub(crate) fn pace_held_until(&mut self, key: &PinKey, until: i64) {
        if let Some(pace) = self.pin_paces.get_mut(key) {
            pace.dispatched_at = pace.dispatched_at.max(until);
        }
    }

    /// The slow-down hold's deadline stands on the pin under `key`, which the pin's next requests rejoin until it
    /// passes, and the request is dispatched there. Nothing for a key with no pace.
    pub(crate) fn stand_slow_down_deadline(&mut self, key: &PinKey, until: i64) {
        if let Some(pace) = self.pin_paces.get_mut(key) {
            pace.deadline = Some(until);
        }
        self.pace_held_until(key, until);
    }

    /// A landing that finds a mark replaces it, since the earlier request's entry is on an account the pin left.
    pub(crate) fn mark_head(&mut self, key: &PinKey, request_id: &str, attempt: u32, at: i64) {
        self.head_marks.insert(
            key.clone(),
            HeadMark {
                request_id: request_id.to_string(),
                attempt,
                at,
            },
        );
    }

    /// The mark under `key` as the pin row spells it, with no resolution applied.
    pub fn head_mark(&self, key: &PinKey) -> Option<&HeadMark> {
        self.head_marks.get(key)
    }

    /// Clears the mark naming `request_id` at `attempt`, or at any attempt under `None`, wherever it stands: at most
    /// one, since a landing replaces the mark. A `Head` report names its leg's attempt, so an earlier leg's report
    /// delivered late never clears the mark a failover's landing set for the same request, while `Ended` names none
    /// and clears whatever the request left.
    pub(crate) fn clear_head_mark(&mut self, request_id: &str, attempt: Option<u32>) -> bool {
        let before = self.head_marks.len();
        self.head_marks.retain(|_, mark| {
            !(mark.request_id == request_id
                && attempt.is_none_or(|attempt| mark.attempt == attempt))
        });
        self.head_marks.len() != before
    }

    /// Clears the mark under `key`, the renewal's and the release's backstop for a report the closed queue refused.
    pub(crate) fn clear_head_mark_of(&mut self, key: &PinKey) -> bool {
        self.head_marks.remove(key).is_some()
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
    /// `WindowShare` a burn is the subscription cost model's anchor share per second, which orders the pins as the
    /// tier's share does, and under `Usd` the pin's fast usage counts beside its row's copy, as
    /// `BurnRates::price_pin` sums them. `None` for a pin burning a type its row cannot price.
    pub fn burn_of(
        &self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        unit: BurnUnit,
        rates: &BurnRates,
        now: i64,
        window_ms: i64,
    ) -> (Vec<(PinRow, Option<f64>)>, f64) {
        let mut pins: Vec<(PinRow, Option<f64>)> = self
            .pins
            .values()
            .filter(|p| p.account_id == account_id && keep(p))
            .map(|p| {
                // Every type decays by the same factor, so decaying the price equals pricing the decayed rates.
                // The pin's last prompt selects the set, so a long-prompt pin ranks at the rates it burns, and a
                // pin never renewed with usage carries zero and prices at the standard rates.
                let burn = rates
                    .price_pin(unit, p, &p.burn_fast)
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

    /// The account's burn per plan window in share per second: the kept pins not named by a live reservation,
    /// each `share.rate` of its plain average on `tier` at `now` decayed with `average_ms`, plus the live
    /// reservations' `burn`. A window a pin's model does not draw is absent, an unpriced pin adds nothing.
    pub fn burn_shares(
        &self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        tier: &str,
        now: i64,
        average_ms: i64,
    ) -> BTreeMap<String, f64> {
        self.shares(account_id, keep, tier, now, average_ms, 1.0)
    }

    /// The twin of `burn_shares` that the slow-down's readers take: the kept pins' sum on every window over `speed`,
    /// the account's realized speed, and the live reservations' `burn` undivided, since a booking is a fresh session's
    /// prior or a moving pin's figure, neither of them slowed on the account yet.
    pub fn demand_shares(
        &self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        tier: &str,
        now: i64,
        average_ms: i64,
        speed: f64,
    ) -> BTreeMap<String, f64> {
        self.shares(account_id, keep, tier, now, average_ms, speed)
    }

    /// The account's rates as `shares` sums them, each over `[now, horizon(window))` as the band weighs it, for the
    /// windows `horizon` names: each kept pin's at its own regime's segments, the pins' sum over `speed`, then each
    /// live booking's undivided at its own regime's segments, its plain rate read under the band of its `booked_at`,
    /// since its `burn` was priced there. An unpriced pin adds nothing.
    #[allow(clippy::too_many_arguments)]
    pub fn banded_shares(
        &self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        tier: &str,
        now: i64,
        average_ms: i64,
        speed: f64,
        horizon: &dyn Fn(&str) -> Option<i64>,
    ) -> BTreeMap<String, Banded> {
        // `Banded::of` is linear in the rate, so the pins of one regime on a window sum first and the span is cut once
        // per regime and window, which keeps the ledger's day walk off the per-pin path.
        let mut rates: Vec<(&'static str, Option<&str>, f64)> = Vec::new();
        self.pin_rates(
            account_id,
            keep,
            tier,
            now,
            average_ms,
            &mut |window, entrypoint, rate| match rates
                .iter_mut()
                .find(|(w, e, _)| *w == window && *e == entrypoint)
            {
                Some((_, _, sum)) => *sum += rate,
                None => rates.push((window, entrypoint, rate)),
            },
        );
        let mut banded: BTreeMap<String, Banded> = BTreeMap::new();
        for (window, entrypoint, rate) in rates {
            let Some(to) = horizon(window) else {
                continue;
            };
            let piece = Banded::of(rate, &self.share.band_segments(window, entrypoint, now, to));
            let sum = banded.entry(window.to_string()).or_default();
            *sum = sum.plus(&piece);
        }
        for sum in banded.values_mut() {
            *sum = sum.divided(speed);
        }
        for reservation in self.live_bookings(account_id) {
            let entrypoint = reservation.entrypoint.as_deref();
            for (window, burn) in &reservation.burn {
                let Some(to) = horizon(window) else {
                    continue;
                };
                let piece = Banded::read_at(
                    *burn,
                    self.share
                        .band_weight_at(window, entrypoint, reservation.booked_at),
                    &self.share.band_segments(window, entrypoint, now, to),
                );
                let sum = banded.entry(window.clone()).or_default();
                *sum = sum.plus(&piece);
            }
        }
        banded
    }

    fn shares(
        &self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        tier: &str,
        now: i64,
        average_ms: i64,
        speed: f64,
    ) -> BTreeMap<String, f64> {
        let mut shares = BTreeMap::new();
        self.pin_rates(
            account_id,
            keep,
            tier,
            now,
            average_ms,
            &mut |window, _, rate| {
                *shares.entry(window.to_string()).or_insert(0.0) += rate;
            },
        );
        for share in shares.values_mut() {
            *share /= speed;
        }
        for reservation in self.live_bookings(account_id) {
            for (window, burn) in &reservation.burn {
                *shares.entry(window.clone()).or_insert(0.0) += burn;
            }
        }
        shares
    }

    /// Folds `add` over each kept pin's decayed rate at `now` on every plan window its model draws, with the pin's
    /// entrypoint, in the pin map's order. A priced pin alone reaches `add`.
    fn pin_rates<'a>(
        &'a self,
        account_id: &str,
        keep: &dyn Fn(&PinRow) -> bool,
        tier: &str,
        now: i64,
        average_ms: i64,
        add: &mut dyn FnMut(&'static str, Option<&'a str>, f64),
    ) {
        // A pin a live booking on this account names counts through the booking's burn alone, so a moved pin counts
        // once while its move is in flight. The booking is matched on the account and the pin row, never on the key
        // across accounts: a booking a request still in flight on the source holds there must not hide the live
        // row another request of the session moved onto the destination.
        let reserved: HashSet<PinKey> = self
            .live_bookings(account_id)
            .filter_map(|reservation| reservation.pin.clone())
            .collect();
        for pin in self
            .pins
            .values()
            .filter(|pin| pin.account_id == account_id && keep(pin))
        {
            // The key clones four strings per kept pin on the selection's hot path, and most selections hold no
            // booking on the account.
            if !reserved.is_empty() && reserved.contains(&pin.key()) {
                continue;
            }
            for window in plan_windows(&pin.model) {
                // The plain average, since the banded copy weighs past renewals' bands and the rate reads the band
                // of now. Every type decays by one factor, so decaying the share equals pricing the decayed rates.
                if let Some(rate) =
                    self.share
                        .rate(window, entrypoint_of(pin), tier, &pin.model, &pin.burn, now)
                {
                    add(
                        window,
                        entrypoint_of(pin),
                        burn_decayed(rate, pin.burn_at, now, average_ms),
                    );
                }
            }
        }
    }

    fn live_bookings<'a>(&'a self, account_id: &'a str) -> impl Iterator<Item = &'a Reservation> {
        self.reservations
            .values()
            .filter(move |reservation| reservation.account_id == account_id)
    }

    /// Whether the pin under the key carries a standing mark: `marked_at` set and `marked_on` its current account.
    pub fn shed_marked(&self, key: &PinKey) -> bool {
        self.pin(key).is_some_and(mark_holds)
    }

    /// The pins on the account whose mark stands, in key order, for the round's `standing` skip.
    pub fn marked_pins(&self, account_id: &str) -> Vec<&PinRow> {
        self.pins_on(account_id, &mark_holds)
    }

    /// The pins on `account_id` that `keep` admits, in key order.
    pub fn pins_on(&self, account_id: &str, keep: &dyn Fn(&PinRow) -> bool) -> Vec<&PinRow> {
        let mut pins: Vec<&PinRow> = self
            .pins
            .values()
            .filter(|p| p.account_id == account_id && keep(p))
            .collect();
        pins.sort_by(|a, b| {
            (&a.key_id, &a.session_id, &a.agent_id, &a.model).cmp(&(
                &b.key_id,
                &b.session_id,
                &b.agent_id,
                &b.model,
            ))
        });
        pins
    }

    /// Marks the pin under the key on the account it sits on, at `at`, for the trigger's `window`, and answers the row
    /// as marked for the write.
    pub(crate) fn mark_pin(&mut self, key: &PinKey, at: i64, window: &str) -> Option<PinRow> {
        let pin = self.pins.get_mut(key)?;
        pin.marked_at = Some(at);
        pin.marked_on = Some(pin.account_id.clone());
        pin.marked_window = Some(window.to_string());
        Some(pin.clone())
    }

    /// Clears the mark of the pin under the key and answers the row as cleared for the write.
    pub(crate) fn clear_mark(&mut self, key: &PinKey) -> Option<PinRow> {
        let pin = self.pins.get_mut(key)?;
        pin.marked_at = None;
        pin.marked_on = None;
        pin.marked_window = None;
        Some(pin.clone())
    }

    /// When `pool_id`'s latest round on the account ran.
    pub fn last_shed(&self, pool_id: &str, account_id: &str) -> Option<i64> {
        self.shed_rounds
            .get(&(pool_id.to_string(), account_id.to_string()))
            .copied()
    }

    /// Writes the pool's round on the account at `at`.
    pub(crate) fn record_shed(&mut self, pool_id: &str, account_id: &str, at: i64) {
        self.shed_rounds
            .insert((pool_id.to_string(), account_id.to_string()), at);
    }

    /// When `window` of the projection test first read over the ceiling on the account under `pool_id`, `None` while it
    /// reads at or under the ceiling or no selection has read it.
    pub fn projection_over_since(
        &self,
        pool_id: &str,
        account_id: &str,
        window: &str,
    ) -> Option<i64> {
        self.projection_over
            .get(&(
                pool_id.to_string(),
                account_id.to_string(),
                window.to_string(),
            ))
            .copied()
    }

    /// Sets the stamp at the first reading over the ceiling, clears it at a reading at or under, and replaces a stamp
    /// older than the window's start (`window_start`: the window's reset less its length where the reading names a
    /// reset, or the reset a 5-hour reading has passed with no sample since).
    pub fn note_projection(
        &mut self,
        pool_id: &str,
        account_id: &str,
        window: &str,
        over: bool,
        window_start: Option<i64>,
        now: i64,
    ) {
        let key = (
            pool_id.to_string(),
            account_id.to_string(),
            window.to_string(),
        );
        if !over {
            self.projection_over.remove(&key);
            return;
        }
        let stamp = self.projection_over.entry(key).or_insert(now);
        // A stamp belongs to one window: a quiet account's stamp from before the reset would fire the new window's
        // first reading over the ceiling at once.
        if window_start.is_some_and(|start| *stamp < start) {
            *stamp = now;
        }
    }

    /// Stamps `window` on the account as over the ceiling since `at`, so a test reaches a window past its wait without
    /// a selection a wait before its instant. No production path stamps but the note, so it is compiled under `test`
    /// and the `debug-hooks` feature alone.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn stamp_projection_over(
        &mut self,
        pool_id: &str,
        account_id: &str,
        window: &str,
        at: i64,
    ) {
        self.projection_over.insert(
            (
                pool_id.to_string(),
                account_id.to_string(),
                window.to_string(),
            ),
            at,
        );
    }

    /// A late failure from account A must not remove a newer pin on account B, so both have to match. Answers the row
    /// the invalidation took, whose figures the failover's landing reads, `None` when nothing matched.
    pub fn invalidate_pin(&mut self, pin: &PinRef, account_id: &str) -> Option<PinRow> {
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
            self.take_pin(&key)
        } else {
            None
        }
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
        self.rekey(spelling, dated_id, at).0
    }

    /// `apply_resolution` with the rows that lost a merge, each leaving with its mark, so the caller clears the
    /// marks that stood on them as a release does.
    pub(crate) fn rekey(
        &mut self,
        spelling: &str,
        dated_id: &str,
        at: i64,
    ) -> (RekeyWrite, Vec<PinRow>) {
        let mut merged_away = Vec::new();
        if let Some(current) = self.resolutions.get(spelling) {
            // A caller whose own bookkeeping of the last move it saw was lost (its future dropped between
            // this state moving and its own record of the move landing) can resubmit a snapshot older than
            // what already stands; walking pins and cooldowns back to it would be a real regression, so the
            // move is ignored and the mapping is reasserted as it already stands.
            if snapshot_date(dated_id) <= snapshot_date(current) {
                let write = RekeyWrite {
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
                return (write, merged_away);
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
            // The pin keeps its epoch under the dated id, so a renewal it had still fences its release there, and
            // its fast usage moves with it.
            let renewed = self.renewed.contains(&spelled);
            // Read ahead of `take_pin`, which drops it: the mark lives as long as its row, so a `Head` report still
            // finds it under the dated id.
            let mark = self.head_marks.remove(&spelled);
            self.take_pin(&spelled);
            // The pace follows the row the merge keeps, so the session's cycle reads on under the dated id.
            let pace = self.pin_paces.remove(&spelled);
            rekey.delete_pins.push(spelled);
            let dated_key = PinKey {
                model: dated_id.to_string(),
                ..pin.key()
            };
            let keep_existing = self
                .pins
                .get(&dated_key)
                .is_some_and(|existing| existing.last_used_at >= pin.last_used_at);
            if keep_existing {
                merged_away.push(pin);
            } else {
                if let Some(pace) = pace {
                    self.pin_paces.insert(dated_key.clone(), pace);
                }
                // A dated row this one replaces leaves with its own mark.
                match mark {
                    Some(mark) => self.head_marks.insert(dated_key.clone(), mark),
                    None => self.head_marks.remove(&dated_key),
                };
                let moved = PinRow {
                    model: dated_id.to_string(),
                    lane: relaned(&pin.lane, spelling, dated_id),
                    ..pin
                };
                if let Some(replaced) = self.pins.insert(dated_key.clone(), moved.clone()) {
                    merged_away.push(replaced);
                }
                if renewed {
                    self.renewed.insert(dated_key.clone());
                } else {
                    self.renewed.remove(&dated_key);
                }
                rekey.upsert_pins.push(moved);
            }
        }
        // A booking names its pin by key, and the suppression in `burn_shares` matches that key against the live
        // row, so the reference follows the row under the dated id or the pin would count beside its booking.
        for reservation in self.reservations.values_mut() {
            if let Some(pin) = reservation.pin.as_mut().filter(|pin| pin.model == spelling) {
                pin.model = dated_id.to_string();
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
        (rekey, merged_away)
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
        // The session ended, so its paces leave with it, one whose pin a failover took and no landing wrote back among
        // them.
        self.pin_paces
            .retain(|key, _| !(key.key_id == key_id && key.session_id == session_id));
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
        // A move writes the key again within the selection that took the row, so a key no pin holds at the tick
        // belongs to a session that ended, or to a failover: the task's report handling takes that row and the landing
        // comes on a later selection, so a tick between the two drops the pace and the landed pin starts a new one.
        let held = &self.pins;
        self.pin_paces.retain(|key, _| held.contains_key(key));
        let slow_downs = self.purge_slow_downs(snapshot, now);
        self.purge_projection_over(snapshot);
        // None of these needs a deletion written: the overage rows cascade with the account and the next holder's
        // first reading replaces one left under another incarnation, the window and credits rows cascade too and
        // the purge tick drops one of another incarnation, and charges and round-robin cursors are held in memory
        // only. Samples, charges and cursors are keyed by id alone, so the reload drops those of a holder the id has
        // moved past.
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
        // A round's time is read within its pool's dwell alone, and a pool that left the registry has no round to
        // read. The marks live on the pins and leave with them, so the purge drops none here.
        self.shed_rounds.retain(|(pool_id, _), at| {
            snapshot
                .pool(pool_id)
                .is_some_and(|pool| now - *at < shed_dwell_ms(pool))
        });
        let ttl_of = |key_id: &str| -> Option<i64> {
            snapshot
                .key(key_id)
                .and_then(|k| snapshot.pool(&k.pool_id))
                .map(affinity_ttl_ms)
        };
        let affinity_entries = self
            .affinity
            .write()
            .expect("affinity lock poisoned")
            .purge(now, ttl_of, |account_id| {
                snapshot.account(account_id).map(|a| a.incarnation)
            });
        let affinity = affinity_entries
            .iter()
            .map(|purged| (purged.model.clone(), purged.hash.clone()))
            .collect();
        self.member_spend
            .retain(|(pool_id, account_id, period), _| {
                member_spend_matches(snapshot, pool_id, account_id, *period)
            });
        Purged {
            affinity,
            affinity_entries,
            cooldowns,
            pins,
            slow_downs,
        }
    }

    /// The slow-down entries a quiet account, a switched-off pool or a passed reset leaves, removed and answered in key
    /// order: an entry whose account holds no pin of the pool, one no selection wrote for `IN_FLIGHT_RECLAIM_MS`, the
    /// backstop the reservations use, one read before its window's passed reset, which every reader already reads as
    /// cleared, and every entry of a pool whose slow-down is off or that left the registry, so a quiet account reads
    /// unslowed again without a selection.
    fn purge_slow_downs(
        &mut self,
        snapshot: &RegistrySnapshot,
        now: i64,
    ) -> Vec<((String, String), SlowDownState)> {
        let pins = &self.pins;
        let mut gone: Vec<(String, String)> = self
            .slow_downs
            .iter()
            .filter(|((pool_id, account_id), entry)| {
                let pool = snapshot.pool(pool_id).filter(|pool| slow_down(pool));
                let pinned = pins.values().any(|pin| {
                    pin.account_id == *account_id
                        && snapshot
                            .key(&pin.key_id)
                            .is_some_and(|key| key.pool_id == *pool_id)
                });
                pool.is_none()
                    || !pinned
                    || now - entry.read_at >= IN_FLIGHT_RECLAIM_MS
                    || self.slow_down_stale(account_id, entry, now)
            })
            .map(|(key, _)| key.clone())
            .collect();
        gone.sort();
        let dropped: Vec<((String, String), SlowDownState)> = gone
            .into_iter()
            .filter_map(|key| self.slow_downs.remove(&key).map(|entry| (key, entry)))
            .collect();
        // No start stands without its end: a dropped span ends at the factor's return, at the passed reset of the
        // window it slowed for where that reset dropped it, or at the last read where the factor never returned.
        for ((pool_id, account_id), entry) in &dropped {
            if let Some(since) = entry.slowed_since {
                let end = span_end(entry, self.stale_reset(account_id, entry, now));
                self.slow_down_edges.push((
                    pool_id.clone(),
                    account_id.clone(),
                    SlowDownEdge::Ended {
                        since,
                        duration_ms: end - since,
                        factor_min: entry.factor_min,
                        held_ms: entry.held_ms,
                    },
                ));
            }
        }
        dropped
    }

    /// The projection stamps no test reads: every stamp of a pool whose shedding is off or that left the registry, the
    /// 5-hour stamps of a pool that slows its pins, whose 5-hour test is the stretch, and the stamps of an account that
    /// holds no pin of the pool, so a flipped switch or a quiet account starts its wait afresh. No age drops a stamp,
    /// since a window that reads over for hours keeps the stamp of its first reading over and the round after the
    /// dwell waits no second time.
    fn purge_projection_over(&mut self, snapshot: &RegistrySnapshot) {
        let pins = &self.pins;
        self.projection_over
            .retain(|(pool_id, account_id, window), _| {
                let Some(pool) = snapshot.pool(pool_id).filter(|pool| load_shedding(pool)) else {
                    return false;
                };
                if window == FIVE_HOUR && slow_down(pool) {
                    return false;
                }
                pins.values().any(|pin| {
                    pin.account_id == *account_id
                        && snapshot
                            .key(&pin.key_id)
                            .is_some_and(|key| key.pool_id == *pool_id)
                })
            });
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
    budget_reached(member, spends, now).map(|(_, end)| end)
}

/// The budget behind `budget_deadline`'s end: the reached period whose end is the latest, with that end.
pub(crate) fn budget_reached(
    member: &PoolMember,
    spends: &[&MemberSpend],
    now: i64,
) -> Option<(BudgetPeriod, i64)> {
    member
        .budgets()
        .filter_map(|(period, budget)| {
            spends
                .iter()
                .find(|s| s.period == period && s.period_start <= now && now < s.period_end)
                .filter(|s| s.spent_usd >= budget)
                .map(|s| (period, s.period_end))
        })
        .max_by_key(|(_, end)| *end)
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
            burn_fast: PinBurn::default(),
            last_input_tokens: 0,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
            lane: crate::lane_key("m", "standard", "none", "-"),
            entrypoint: String::new(),
            marked_at: None,
            marked_on: None,
            marked_window: None,
        };
        (pin, Some(burn))
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
            request_id: "r-1".into(),
            seq: 1,
            attempt: 1,
            message_id: None,
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
    fn a_pins_fast_usage_moves_with_a_rekey_leaves_with_the_pin_and_never_reaches_a_fresh_pin_under_its_key(
    ) {
        let (pin, _) = burning("s", 1.0);
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![pin.clone()],
            ..RoutingLoad::default()
        });
        let fast = PinBurn {
            input: 2.0,
            ..PinBurn::default()
        };
        state.set_fast_burn(&pin.key(), fast);
        state.apply_resolution("m", "m-20260101", 0);
        let dated = PinKey {
            model: "m-20260101".into(),
            ..pin.key()
        };
        assert_eq!(
            state.fast_burns().into_iter().collect::<Vec<_>>(),
            vec![(dated.clone(), fast)],
            "the rekey carries the fast usage to the dated id"
        );
        state.put_pin(PinRow {
            epoch: 2,
            ..pin.clone()
        });
        assert_eq!(
            state.fast_burn(&dated),
            PinBurn::default(),
            "a fresh pin under the key starts without the replaced pin's fast usage"
        );
        state.set_fast_burn(&dated, fast);
        state.remove_pin(&dated);
        assert!(
            state.fast_burns().is_empty(),
            "the fast usage leaves with the pin"
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
            long_context: HashMap::new(),
            tiers: HashMap::new(),
            fallback: RateTable::unit().fallback,
        };
        let input_only = PinBurn {
            input: 3.0,
            ..PinBurn::default()
        };
        assert_eq!(table.price_burn("claude-guess", &input_only, 0), Some(6.0));
        let with_reads = PinBurn {
            input: 3.0,
            cache_read: 1.0,
            ..PinBurn::default()
        };
        assert_eq!(
            table.price_burn("claude-guess", &with_reads, 0),
            None,
            "a burned type without a rate"
        );
        assert_eq!(
            table.price(
                "claude-guess",
                PinUsage {
                    input: 1,
                    ..PinUsage::default()
                },
                0
            ),
            Some(2.0)
        );
        assert_eq!(
            table.price(
                "claude-guess",
                PinUsage {
                    cache_read: 1,
                    ..PinUsage::default()
                },
                0
            ),
            None
        );
        assert_eq!(
            table.price(
                "gpt-5",
                PinUsage {
                    cache_read: 1,
                    ..PinUsage::default()
                },
                0
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
            long_context: HashMap::new(),
            tiers: HashMap::new(),
            fallback: flat(1.0),
        };
        let usage = PinUsage {
            input: 10,
            cache_read: 100,
            write_5m: 4,
            write_1h: 2,
            output: 3,
        };
        let prompt = usage.input + usage.cache_read + usage.write_5m + usage.write_1h;
        assert_eq!(
            table.price("claude-sonnet-4-5-20250929", usage, prompt),
            Some(30.0 + 25.0 + 15.0 + 12.0 + 45.0),
            "the dated id prices at the undated sonnet row"
        );
        assert_eq!(
            table.price("claude-fable-5-1-20260601", usage, prompt),
            Some(50.0 + 50.0 + 25.0 + 20.0 + 75.0),
            "and the dated fable id at its own row, not the fallback"
        );
        assert_eq!(
            table.price("claude-opus-5-20260101", usage, prompt),
            Some(10.0 + 100.0 + 4.0 + 2.0 + 3.0),
            "a model no row names prices at the fallback"
        );
    }

    #[test]
    fn an_unlisted_id_reads_the_row_of_the_model_tier_its_id_names_before_the_fallback() {
        let flat = |r: f64| Rates {
            input: Some(r),
            cache_read: Some(r),
            write_5m: Some(r),
            write_1h: Some(r),
            output: Some(r),
        };
        let table = RateTable {
            rows: HashMap::from([("claude-opus-5-5".to_string(), flat(1.0))]),
            long_context: HashMap::new(),
            tiers: HashMap::from([("opus".to_string(), flat(2.0))]),
            fallback: flat(10.0),
        };
        assert_eq!(
            table.rates_for("claude-opus-5-5", 0),
            flat(1.0),
            "a listed row wins over its model tier"
        );
        assert_eq!(
            table.rates_for("claude-opus-9", 0),
            flat(2.0),
            "an unlisted id reads the model tier it names"
        );
        assert_eq!(
            table.rates_for("claude-opus-9-20270101", 0),
            flat(2.0),
            "a dated unlisted id reads the model tier its undated id names"
        );
        assert_eq!(
            table.rates_for("claude-sonnet-9", 0),
            flat(10.0),
            "a model tier without a row reads the fallback"
        );
        assert_eq!(
            table.rates_for("gpt-5", 0),
            flat(10.0),
            "an id naming no model tier reads the fallback"
        );
    }

    #[test]
    fn a_long_context_row_prices_a_prompt_over_its_threshold_at_its_own_rates() {
        let flat = |r: f64| Rates {
            input: Some(r),
            cache_read: Some(r),
            write_5m: Some(r),
            write_1h: Some(r),
            output: Some(r),
        };
        // An undated row with a long-context set beside an exact dated row without one: the dated snapshot is
        // priced its own way.
        let table = RateTable {
            rows: HashMap::from([
                ("claude-haiku-5-5".to_string(), flat(1.0)),
                ("claude-haiku-5-5-20260101".to_string(), flat(2.0)),
            ]),
            long_context: HashMap::from([(
                "claude-haiku-5-5".to_string(),
                LongContextRates {
                    over: 1_000,
                    rates: flat(5.0),
                },
            )]),
            tiers: HashMap::new(),
            fallback: flat(10.0),
        };
        let one = PinUsage {
            input: 1,
            ..PinUsage::default()
        };
        assert_eq!(
            table.price("claude-haiku-5-5", one, 1_000),
            Some(1.0),
            "a prompt of exactly `over` reads the row"
        );
        assert_eq!(
            table.price("claude-haiku-5-5", one, 1_001),
            Some(5.0),
            "one token more reads the long-context rates"
        );
        assert_eq!(
            table.price("claude-haiku-5-5-20260301", one, 1_001),
            Some(5.0),
            "a dated id with no row of its own reads its undated long-context row"
        );
        assert_eq!(
            table.price("claude-haiku-5-5-20260101", one, 1_000),
            Some(2.0),
            "an exact dated row wins at the threshold"
        );
        assert_eq!(
            table.price("claude-haiku-5-5-20260101", one, 1_001),
            Some(2.0),
            "and over it, the key resolved as `PriceTable::entry` resolves it, never the undated row's long-context rates"
        );
        assert_eq!(
            table.price("gpt-5", one, 1_001),
            Some(10.0),
            "an unlisted model reads the fallback whatever its prompt"
        );
        let burn = PinBurn {
            input: 2.0,
            ..PinBurn::default()
        };
        assert_eq!(
            table.price_burn("claude-haiku-5-5", &burn, 1_001),
            Some(10.0)
        );
        assert!(RateTable::unit().long_context.is_empty());
    }

    #[test]
    fn a_head_sample_for_one_model_keeps_the_buckets_of_another_and_an_older_reading_never_replaces_a_newer_one(
    ) {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        let reading = |utilization: f64, observed_at: i64| WindowReading {
            utilization,
            reset: None,
            observed_at,
            source: WindowSource::Head,
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
            source: WindowSource::Head,
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
                    state_cause: StateCause::Operator,
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
                source: WindowSource::Head,
            },
        )
        .with(
            "itpm:y",
            WindowReading {
                utilization: 0.3,
                reset: None,
                observed_at: 1_000,
                source: WindowSource::Head,
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
                    source: WindowSource::Poll,
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
                        source: WindowSource::Predicted,
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
                source: WindowSource::Predicted,
            }),
            "at the reset the window reads zero, and the next reading names the reset after it"
        );
        state.charge("a", 6_000, &[(FIVE_HOUR, 0.125), (SEVEN_DAY, 0.0)]);
        assert!(
            (utilization(&state, FIVE_HOUR, 7_000) - 0.125).abs() < CLOSE,
            "a request completed since the reset counts, and the one before it does not"
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
                    source: WindowSource::Poll,
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
                source: WindowSource::Poll,
            }),
            "no reset named: the reading stands however old it is"
        );
        read(&mut state, FIVE_HOUR, 0.2, None, 2_000);
        assert!((utilization(&state, FIVE_HOUR, 1_000 + 3 * WEEK_MS) - 0.2).abs() < CLOSE);
    }

    #[test]
    fn a_reading_restored_at_start_reads_stored_and_a_charge_turns_it_predicted() {
        let mut state = RoutingState::from_load(RoutingLoad {
            windows: vec![phoebis_store::WindowRow {
                account_id: "a".into(),
                incarnation: 1,
                observed_at: 1_000,
                reset: Some(HOUR_MS),
                utilization: 0.5,
                window: FIVE_HOUR.into(),
            }],
            ..RoutingLoad::default()
        });
        assert_eq!(
            state.predicted("a", FIVE_HOUR, 2_000),
            Some(WindowReading {
                utilization: 0.5,
                reset: Some(HOUR_MS),
                observed_at: 1_000,
                source: WindowSource::Stored,
            }),
            "the registry keeps no origin, and nothing has moved the restored reading"
        );
        state.charge("a", 3_000, &[(FIVE_HOUR, 0.25)]);
        let moved = state
            .predicted("a", FIVE_HOUR, 4_000)
            .expect("the account holds a reading of the window");
        assert_eq!(
            moved.source,
            WindowSource::Predicted,
            "the router's own use since the reading moved it"
        );
        assert!((moved.utilization - 0.75).abs() < CLOSE);
    }

    #[test]
    fn a_reading_past_its_reset_is_predicted_from_the_router_s_own_use() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        read(&mut state, FIVE_HOUR, 0.5, Some(5_000), 1_000);
        assert_eq!(
            state.predicted("a", FIVE_HOUR, 4_000).map(|r| r.source),
            Some(WindowSource::Poll),
            "before its reset, with nothing charged, the reading stands as the poll read it"
        );
        assert_eq!(
            state.predicted("a", FIVE_HOUR, 5_000),
            Some(WindowReading {
                utilization: 0.0,
                reset: None,
                observed_at: 1_000,
                source: WindowSource::Predicted,
            }),
            "at the reset the window reads zero, the router's own figure"
        );
        state.charge("a", 6_000, &[(FIVE_HOUR, 0.125)]);
        assert_eq!(
            state.predicted("a", FIVE_HOUR, 7_000),
            Some(WindowReading {
                utilization: 0.125,
                reset: None,
                observed_at: 1_000,
                source: WindowSource::Predicted,
            }),
            "past the reset the window reads the share completed since it"
        );
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
                            source: WindowSource::Poll,
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

    /// A booking on `account` under `attempt`, standing from `booked_at` to `expires_at`.
    fn reservation(
        account: &str,
        attempt: u32,
        pin: Option<PinKey>,
        rewrite: &[(&str, f64)],
        burn: &[(&str, f64)],
        booked_at: i64,
        expires_at: i64,
    ) -> Reservation {
        let map = |entries: &[(&str, f64)]| -> BTreeMap<String, f64> {
            entries.iter().map(|(w, s)| (w.to_string(), *s)).collect()
        };
        Reservation {
            account_id: account.into(),
            attempt,
            pin,
            entrypoint: None,
            booked_at,
            rewrite: map(rewrite),
            burn: map(burn),
            expires_at,
        }
    }

    /// A poll's reading of the 5-hour window alone, with no reset.
    fn five_hour_reading(utilization: f64, observed_at: i64) -> UtilizationSample {
        UtilizationSample::default().with(
            FIVE_HOUR,
            WindowReading {
                utilization,
                reset: None,
                observed_at,
                source: WindowSource::Poll,
            },
        )
    }

    fn close(a: f64, b: f64) -> bool {
        (a - b).abs() < 1e-9
    }

    /// `burn.input` priced a millionth of the 5-hour window and a ten-millionth of every other window per token a
    /// second, whatever the tier or the model.
    fn input_rates() -> SharePricer {
        SharePricer::default().with_rate(|window, _, _, _, burn, _| {
            Some(if window == FIVE_HOUR {
                burn.input * 1e-6
            } else {
                burn.input * 1e-7
            })
        })
    }

    #[test]
    fn the_default_pricer_prices_no_equivalent() {
        assert_eq!(
            SharePricer::default().equivalent(SEVEN_DAY, None, "m", PinUsage::default(), 0),
            None
        );
    }

    #[test]
    fn a_reservation_counts_in_the_prediction_until_it_is_released_and_a_second_booking_replaces_the_first(
    ) {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.record_sample("a", five_hour_reading(0.5, 1_000));
        state.record_sample("b", five_hour_reading(0.2, 1_000));
        let five_hour = |state: &RoutingState, account: &str| {
            state
                .predicted(account, FIVE_HOUR, 2_000)
                .expect("a reading")
        };
        let booking = |account: &str, attempt: u32| {
            reservation(
                account,
                attempt,
                None,
                &[(FIVE_HOUR, 0.1)],
                &[],
                2_000,
                3_000,
            )
        };
        state.book_reservation("r", booking("a", 1));
        let raised = five_hour(&state, "a");
        assert!(
            close(raised.utilization, 0.6) && raised.source == WindowSource::Predicted,
            "the booking's rewrite joins the reading and names the prediction: {raised:?}"
        );
        assert!(state.release_reservation("r").is_some());
        let back = five_hour(&state, "a");
        assert!(
            close(back.utilization, 0.5) && back.source == WindowSource::Poll,
            "the reading returns once the booking leaves: {back:?}"
        );
        state.book_reservation("r", booking("a", 1));
        assert!(state.release_reservation("r").is_some());
        state.charge("a", 2_500, &[(FIVE_HOUR, 0.08)]);
        assert!(
            close(five_hour(&state, "a").utilization, 0.58),
            "the charge alone, the released booking gone"
        );
        assert!(state.reservation("r").is_none() && state.release_reservation("r").is_none());
        state.book_reservation("r", booking("a", 1));
        state.book_reservation("r", booking("b", 2));
        let booked = state
            .reservation("r")
            .expect("one booking under the request");
        assert_eq!(
            (booked.account_id.as_str(), booked.attempt),
            ("b", 2),
            "a failover's selection replaces the booking under the request id"
        );
        assert_eq!(state.reserved("a", FIVE_HOUR), 0.0);
        assert!(close(state.reserved("b", FIVE_HOUR), 0.1));
        assert!(state.reservations_on("a").is_empty());
        assert_eq!(state.reservations_on("b").len(), 1);
    }

    #[test]
    fn expired_reservations_leave_at_their_instant_and_a_restart_loads_none() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.book_reservation(
            "r1",
            reservation("a", 1, None, &[(FIVE_HOUR, 0.1)], &[], 0, 10),
        );
        state.book_reservation(
            "r2",
            reservation("a", 1, None, &[(FIVE_HOUR, 0.1)], &[], 0, 20),
        );
        let expired = state.expire_reservations(15);
        assert_eq!(
            expired
                .iter()
                .map(|(id, _)| id.as_str())
                .collect::<Vec<_>>(),
            ["r1"]
        );
        assert!(state.reservation("r1").is_none() && state.reservation("r2").is_some());
        let mut loaded = RoutingState::from_load(RoutingLoad {
            pins: vec![burning("s", 1.0).0],
            ..RoutingLoad::default()
        });
        assert!(
            loaded.reservations_on("a").is_empty()
                && loaded.expire_reservations(i64::MAX).is_empty(),
            "a load carries no booking"
        );
    }

    #[test]
    fn burn_shares_prices_each_kept_pin_per_window_leaves_a_reserved_pin_out_and_adds_the_reservations_burn(
    ) {
        let (heavy, _) = burning("heavy", 40_000.0);
        let (light, _) = burning("light", 10_000.0);
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![heavy, light.clone()],
            ..RoutingLoad::default()
        });
        state.set_share(input_rates());
        state.book_reservation(
            "r",
            reservation("a", 1, Some(light.key()), &[], &[(FIVE_HOUR, 0.002)], 0, 10),
        );
        let shares = state.burn_shares("a", &|_| true, "t", 0, 1_800_000);
        assert_eq!(
            shares.keys().map(String::as_str).collect::<Vec<_>>(),
            [FIVE_HOUR, SEVEN_DAY]
        );
        assert!(
            close(shares[FIVE_HOUR], 0.04 + 0.002),
            "the heavy pin priced, the reserved light pin left out, the booking's burn added: {shares:?}"
        );
        assert!(close(shares[SEVEN_DAY], 0.004), "{shares:?}");
        assert!(state.release_reservation("r").is_some());
        state.set_share(SharePricer::default().with_rate(|_, _, _, _, _, _| None));
        assert!(
            state
                .burn_shares("a", &|_| true, "t", 0, 1_800_000)
                .is_empty(),
            "an unpriced pin adds nothing"
        );
    }

    #[test]
    fn burn_shares_decays_a_pins_share_from_its_last_fold_by_the_average_constant() {
        // One pin folded one average constant ago: its share reads as stored times e to the minus one.
        let (pin, _) = burning("aging", 10_000.0);
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![PinRow {
                burn_at: Some(0),
                ..pin
            }],
            ..RoutingLoad::default()
        });
        state.set_share(input_rates());
        let shares = state.burn_shares("a", &|_| true, "t", 1_800_000, 1_800_000);
        assert!(
            close(shares[FIVE_HOUR], 0.01 * (-1.0f64).exp()),
            "the share decays by one factor over one constant: {shares:?}"
        );
        let fresh = state.burn_shares("a", &|_| true, "t", 0, 1_800_000);
        assert!(
            close(fresh[FIVE_HOUR], 0.01),
            "no time passed, no decay: {fresh:?}"
        );
    }

    /// The start `window_started` reads at `now` from one reading of `window` naming `reset`, observed an hour before
    /// the earlier of the reset and `now`.
    fn started(window: &str, reset: Option<i64>, now: i64) -> Option<i64> {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        let observed_at = reset.map_or(now, |reset| reset.min(now)) - HOUR_MS;
        read(&mut state, window, 0.5, reset, observed_at);
        state.window_started("a", window, now)
    }

    #[test]
    fn window_started_reads_the_reset_less_the_length_ahead_the_passed_reset_behind_and_none_without_one(
    ) {
        let now = 10 * WEEK_MS;
        let day = 24 * HOUR_MS;
        let ahead = now + HOUR_MS;
        assert_eq!(
            started(FIVE_HOUR, Some(ahead), now),
            Some(ahead - crate::select::FIVE_HOUR_MS),
            "a 5-hour reset ahead"
        );
        let behind = now - 3_000;
        assert_eq!(
            started(FIVE_HOUR, Some(behind), now),
            Some(behind),
            "a passed 5-hour reset starts the window the next request opened"
        );
        assert_eq!(
            started(FIVE_HOUR, Some(now), now),
            Some(now),
            "a 5-hour reset at now has passed"
        );
        let next_week = now + day;
        assert_eq!(
            started(SEVEN_DAY, Some(next_week), now),
            Some(next_week - WEEK_MS),
            "a weekly reset ahead"
        );
        // A reset nine days behind rolls forward two whole weeks to the first reset after now, and the window started a
        // week before that one.
        let last_week = now - 9 * day;
        assert_eq!(
            started(SEVEN_DAY, Some(last_week), now),
            Some(last_week + 2 * WEEK_MS - WEEK_MS),
            "a passed weekly reset"
        );
        assert_eq!(
            started(FIVE_HOUR, None, now),
            None,
            "a reading naming no reset"
        );
        assert_eq!(
            RoutingState::from_load(RoutingLoad::default()).window_started("a", FIVE_HOUR, now),
            None,
            "an account with no reading"
        );
    }

    #[test]
    fn window_started_reads_a_start_ahead_of_now_for_a_reset_more_than_a_windows_length_ahead() {
        let now = 10 * WEEK_MS;
        let day = 24 * HOUR_MS;
        let reset = now + 9 * HOUR_MS;
        assert_eq!(
            started(FIVE_HOUR, Some(reset), now),
            Some(reset - crate::select::FIVE_HOUR_MS),
            "four hours after now"
        );
        let reset = now + 9 * day;
        assert_eq!(
            started(SEVEN_DAY, Some(reset), now),
            Some(reset - WEEK_MS),
            "two days after now"
        );
        assert_eq!(
            started(FIVE_HOUR, Some(now + crate::select::FIVE_HOUR_MS), now),
            Some(now),
            "a reset exactly a window's length ahead"
        );
    }

    /// The pace key the fold cases arrive under.
    fn paced() -> PinKey {
        PinKey {
            key_id: "k".into(),
            session_id: "s".into(),
            agent_id: "main".into(),
            model: "m".into(),
        }
    }

    #[test]
    fn fold_pace_counts_its_gaps_to_eight_and_holds_the_count_there() {
        const AVERAGE_MS: i64 = 1_800_000;
        let mut state = RoutingState::from_load(RoutingLoad::default());
        let read = |state: &RoutingState| {
            let pace = state.pin_pace(&paced()).expect("a pace");
            (pace.gaps, pace.cycle_ms)
        };
        state.fold_pace(&paced(), 0, AVERAGE_MS);
        assert_eq!(
            read(&state),
            (0, None),
            "the first arrival starts the pace with no gap"
        );
        for gap in 1..=8 {
            state.fold_pace(&paced(), i64::from(gap) * 30_000, AVERAGE_MS);
            assert_eq!(read(&state), (gap, Some(30_000.0)), "gap {gap}");
        }
        state.fold_pace(&paced(), 246_000, AVERAGE_MS);
        assert_eq!(
            read(&state),
            (CYCLE_GAPS, Some(27_000.0)),
            "a ninth gap leaves the count at eight and moves the cycle by an eighth"
        );
    }

    #[test]
    fn fold_pace_leaves_the_cycle_and_the_count_standing_over_a_pause() {
        const AVERAGE_MS: i64 = 1_800_000;
        let mut state = RoutingState::from_load(RoutingLoad::default());
        let read = |state: &RoutingState| {
            let pace = state.pin_pace(&paced()).expect("a pace");
            (pace.gaps, pace.cycle_ms, pace.dispatched_at)
        };
        for gap in 0..=3 {
            state.fold_pace(&paced(), gap * 30_000, AVERAGE_MS);
        }
        assert_eq!(read(&state), (3, Some(30_000.0), 90_000));
        let paused = 90_000 + AVERAGE_MS;
        state.fold_pace(&paced(), paused, AVERAGE_MS);
        assert_eq!(
            read(&state),
            (3, Some(30_000.0), paused),
            "a gap of the burn average folds nothing and dates the next gap"
        );
        let next = paused + AVERAGE_MS - 1;
        state.fold_pace(&paced(), next, AVERAGE_MS);
        assert_eq!(
            read(&state),
            (
                4,
                Some(30_000.0 + (AVERAGE_MS - 1 - 30_000) as f64 / 4.0),
                next
            ),
            "a gap one millisecond under the burn average folds"
        );
    }

    fn head_mark_of(request_id: &str, attempt: u32, at: i64) -> HeadMark {
        HeadMark {
            request_id: request_id.into(),
            attempt,
            at,
        }
    }

    #[test]
    fn mark_head_replaces_a_standing_mark_with_the_later_request() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.mark_head(&paced(), "r-1", 1, 1_000);
        assert_eq!(
            state.head_mark(&paced()),
            Some(&head_mark_of("r-1", 1, 1_000))
        );
        state.mark_head(&paced(), "r-2", 2, 2_000);
        assert_eq!(
            state.head_mark(&paced()),
            Some(&head_mark_of("r-2", 2, 2_000)),
            "the later landing's request wins, its attempt with it"
        );
    }

    #[test]
    fn clear_head_mark_matches_the_request_and_its_attempt() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        state.mark_head(&paced(), "r-1", 2, 1_000);
        assert!(!state.clear_head_mark("r-2", None), "another request");
        assert!(
            !state.clear_head_mark("r-1", Some(1)),
            "an earlier leg of the request"
        );
        assert!(state.head_mark(&paced()).is_some(), "the mark stands");
        assert!(state.clear_head_mark("r-1", Some(2)));
        assert_eq!(state.head_mark(&paced()), None);
        state.mark_head(&paced(), "r-1", 3, 2_000);
        assert!(
            state.clear_head_mark("r-1", None),
            "no attempt clears the request's mark whatever its attempt"
        );
        assert!(!state.clear_head_mark("r-1", None), "once cleared");
    }

    #[test]
    fn clear_head_mark_of_clears_the_key() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        assert!(!state.clear_head_mark_of(&paced()));
        state.mark_head(&paced(), "r-1", 1, 1_000);
        assert!(state.clear_head_mark_of(&paced()));
        assert_eq!(state.head_mark(&paced()), None);
    }

    #[test]
    fn take_pin_drops_the_mark() {
        let (pin, _) = burning("s", 1.0);
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![pin.clone()],
            ..RoutingLoad::default()
        });
        state.mark_head(&pin.key(), "r-1", 1, 1_000);
        state.remove_pin(&pin.key());
        assert_eq!(state.head_mark(&pin.key()), None);
    }

    #[test]
    fn a_booking_left_on_the_source_by_a_request_in_flight_hides_no_live_pin_on_the_destination() {
        // Two requests share the pin: r1 is placed on a and stays in flight there while r2 fails over to b, where
        // the pin's row now sits, and completes.
        let (pin, _) = burning("shared", 10_000.0);
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![pin.clone()],
            ..RoutingLoad::default()
        });
        state.set_share(input_rates());
        state.book_reservation(
            "r1",
            reservation("a", 1, Some(pin.key()), &[], &[(FIVE_HOUR, 0.01)], 0, 10),
        );
        state.remove_pin(&pin.key());
        state.put_pin(PinRow {
            account_id: "b".into(),
            epoch: 2,
            ..pin.clone()
        });
        state.book_reservation(
            "r2",
            reservation("b", 2, Some(pin.key()), &[], &[(FIVE_HOUR, 0.01)], 0, 10),
        );
        assert!(
            state.release_reservation("r2").is_some(),
            "r2's usage releases its booking"
        );
        let on_b = state.burn_shares("b", &|_| true, "t", 0, 1_800_000);
        assert!(
            close(on_b.get(FIVE_HOUR).copied().unwrap_or(0.0), 0.01),
            "b's live pin counts though r1's booking on a names its key: {on_b:?}"
        );
        let on_a = state.burn_shares("a", &|_| true, "t", 0, 1_800_000);
        assert!(
            close(on_a[FIVE_HOUR], 0.01),
            "a carries r1's booking alone: {on_a:?}"
        );
    }

    #[test]
    fn a_rekey_moves_a_bookings_pin_with_the_row_so_the_pin_still_counts_once() {
        let (pin, _) = burning("undated", 10_000.0);
        let mut state = RoutingState::from_load(RoutingLoad {
            pins: vec![pin.clone()],
            ..RoutingLoad::default()
        });
        state.set_share(input_rates());
        state.book_reservation(
            "r",
            reservation("a", 1, Some(pin.key()), &[], &[(FIVE_HOUR, 0.01)], 0, 10),
        );
        let (write, _) = state.rekey("m", "m-20260101", 5);
        assert_eq!(
            write.upsert_pins.len(),
            1,
            "the pin moves under the dated id"
        );
        let shares = state.burn_shares("a", &|_| true, "t", 0, 1_800_000);
        assert!(
            close(shares[FIVE_HOUR], 0.01),
            "the booking's burn alone, the rekeyed pin left out: {shares:?}"
        );
        assert_eq!(
            state
                .reservation("r")
                .and_then(|r| r.pin.as_ref())
                .map(|p| p.model.as_str()),
            Some("m-20260101"),
            "the booking names the pin by its new key"
        );
    }

    #[test]
    fn a_models_week_is_a_burn_window_only_for_the_models_that_draw_it() {
        let windows_of = |model: &str| {
            let (pin, _) = burning("s", 1.0);
            let mut state = RoutingState::from_load(RoutingLoad {
                pins: vec![PinRow {
                    model: model.into(),
                    ..pin
                }],
                ..RoutingLoad::default()
            });
            state.set_share(
                SharePricer::default().with_rate(|_, _, _, _, burn, _| Some(burn.input)),
            );
            state
                .burn_shares("a", &|_| true, "t", 0, 1_800_000)
                .into_keys()
                .collect::<Vec<_>>()
        };
        assert_eq!(
            windows_of("claude-fable-5-1"),
            [FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE],
            "a Fable pin burns its own week beside the shared windows"
        );
        assert_eq!(windows_of("claude-sonnet-5"), [FIVE_HOUR, SEVEN_DAY]);
    }

    #[test]
    fn a_realized_speed_that_is_not_finite_or_not_positive_reads_as_full_speed() {
        for realized in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -0.25] {
            assert_eq!(full_speed_unless_positive(realized), 1.0, "{realized}");
        }
        assert_eq!(full_speed_unless_positive(0.25), 0.25);
    }

    #[test]
    fn a_flicker_around_one_inside_the_wait_makes_one_span() {
        const SECOND: i64 = 1_000;
        const MINUTE: i64 = 60 * SECOND;
        const T: i64 = 10 * 3_600_000;
        let read = |factor: f64| SlowDownRead {
            utilization: 0.9,
            horizon: T + 40 * MINUTE,
            reset: Some(T + 40 * MINUTE),
            demand: 0.1 / 3_600.0,
            banded: Banded::flat(0.1 / 3_600.0, T, T + 40 * MINUTE),
            stretch_ms: 10 * MINUTE,
            threshold_ms: 30 * MINUTE,
            factor,
            ramp: 1.0,
            ramped: factor,
            speed: 1.0,
            plan: Vec::new(),
            realized: 1.0,
        };
        let mut state = RoutingState::from_load(RoutingLoad::default());
        for (at, factor) in [
            (T, 0.6),
            (T + MINUTE, 1.0),
            (T + 90 * SECOND, 0.4),
            (T + 2 * MINUTE, 1.0),
        ] {
            state.write_slow_down("p", "a", &read(factor), 1.0, at, 1_800_000);
        }
        assert_eq!(
            state.take_slow_down_edges(T + 2 * MINUTE + SLOW_DOWN_WAIT_MS),
            vec![
                (
                    "p".to_string(),
                    "a".to_string(),
                    SlowDownEdge::Started {
                        factor: 0.6,
                        stretch_ms: 10 * MINUTE,
                        threshold_ms: 30 * MINUTE,
                        demand: read(0.6).demand * 3_600.0,
                        reset: Some(T + 40 * MINUTE),
                    }
                ),
                (
                    "p".to_string(),
                    "a".to_string(),
                    SlowDownEdge::Ended {
                        since: T,
                        duration_ms: 2 * MINUTE,
                        factor_min: 0.4,
                        held_ms: 0,
                    }
                ),
            ],
            "the dip inside the wait continues the span, which ends once at the last return"
        );
        let entry = state.slow_down_state("p", "a").expect("the entry stays");
        assert_eq!(
            (
                entry.slowed_since,
                entry.clear_since,
                entry.factor_min,
                entry.held_ms
            ),
            (None, Some(T + 2 * MINUTE), 1.0, 0),
            "the closed span leaves its figures reset and the return's instant"
        );
        assert!(
            state
                .take_slow_down_edges(T + 2 * MINUTE + SLOW_DOWN_WAIT_MS + MINUTE)
                .is_empty(),
            "a span ends once"
        );
    }

    #[test]
    fn a_dip_once_the_factor_has_read_one_for_the_wait_ends_the_span_and_opens_another() {
        const MINUTE: i64 = 60_000;
        const T: i64 = 10 * 3_600_000;
        let read = |factor: f64| SlowDownRead {
            utilization: 0.9,
            horizon: T + 40 * MINUTE,
            reset: Some(T + 40 * MINUTE),
            demand: 0.1 / 3_600.0,
            banded: Banded::flat(0.1 / 3_600.0, T, T + 40 * MINUTE),
            stretch_ms: 10 * MINUTE,
            threshold_ms: 30 * MINUTE,
            factor,
            ramp: 1.0,
            ramped: factor,
            speed: 1.0,
            plan: Vec::new(),
            realized: 1.0,
        };
        let started = |factor: f64| SlowDownEdge::Started {
            factor,
            stretch_ms: 10 * MINUTE,
            threshold_ms: 30 * MINUTE,
            demand: read(factor).demand * 3_600.0,
            reset: Some(T + 40 * MINUTE),
        };
        let mut state = RoutingState::from_load(RoutingLoad::default());
        // No drain runs between the return and the dip, as when a selection lands before the purge tick after the wait.
        let dip = T + MINUTE + SLOW_DOWN_WAIT_MS;
        for (at, factor) in [(T, 0.6), (T + MINUTE, 1.0), (dip, 0.5)] {
            state.write_slow_down("p", "a", &read(factor), 1.0, at, 1_800_000);
        }
        let edge = |edge: SlowDownEdge| ("p".to_string(), "a".to_string(), edge);
        assert_eq!(
            state.take_slow_down_edges(dip),
            vec![
                edge(started(0.6)),
                edge(SlowDownEdge::Ended {
                    since: T,
                    duration_ms: MINUTE,
                    factor_min: 0.6,
                    held_ms: 0,
                }),
                edge(started(0.5)),
            ],
            "the factor read 1 for the wait, so the span ended at the return and the dip is a span of its own"
        );
        let entry = state.slow_down_state("p", "a").expect("the entry");
        assert_eq!(
            (entry.slowed_since, entry.clear_since, entry.factor_min),
            (Some(dip), None, 0.5)
        );
    }

    /// A registry that holds no pool, so the purge drops every slow-down entry.
    fn no_pools() -> RegistrySnapshot {
        RegistrySnapshot::from_load(phoebis_store::RegistryLoad::default(), [0; 32], 0, 1)
    }

    /// A read of `factor` with nothing over the threshold, for the cases that write an entry and read it back.
    fn read_of(factor: f64) -> SlowDownRead {
        SlowDownRead {
            utilization: 0.9,
            horizon: 3_600_000,
            reset: Some(3_600_000),
            demand: 0.1 / 3_600.0,
            banded: Banded::flat(0.1 / 3_600.0, 0, 3_600_000),
            stretch_ms: 600_000,
            threshold_ms: 1_800_000,
            factor,
            ramp: 1.0,
            ramped: factor,
            speed: 1.0,
            plan: Vec::new(),
            realized: 1.0,
        }
    }

    #[test]
    fn the_speed_folds_toward_the_realized_speed_by_the_time_since_the_last_read_over_the_burn_average(
    ) {
        const AVERAGE_MS: i64 = 1_800_000;
        let mut state = RoutingState::from_load(RoutingLoad::default());
        let first = state.write_slow_down("p", "a", &read_of(0.5), 0.5, 0, AVERAGE_MS);
        assert_eq!(
            first.speed, 0.5,
            "the first write takes the realized speed whole"
        );
        let speed = state
            .write_slow_down("p", "a", &read_of(0.5), 1.0, AVERAGE_MS / 2, AVERAGE_MS)
            .speed;
        let w = (-0.5_f64).exp();
        assert!(
            (speed - (0.5 * w + 1.0 * (1.0 - w))).abs() < 1e-12,
            "half a burn average after the last read keeps e^-0.5 of the old speed: {speed}"
        );
    }

    #[test]
    fn a_span_the_purge_drops_after_a_return_to_one_ends_at_the_return() {
        const MINUTE: i64 = 60_000;
        let mut state = RoutingState::from_load(RoutingLoad::default());
        // The factor returns to 1 a minute in and reads 1 again half a minute later, inside the wait.
        for (at, factor) in [(0, 0.6), (MINUTE, 1.0), (90_000, 1.0)] {
            state.write_slow_down("p", "a", &read_of(factor), 1.0, at, 1_800_000);
        }
        // The pool left the registry, so the purge drops the entry before the wait closes the span.
        state.purge_slow_downs(&no_pools(), 100_000);
        let ended: Vec<SlowDownEdge> = state
            .take_slow_down_edges(100_000)
            .into_iter()
            .map(|(_, _, edge)| edge)
            .filter(|edge| matches!(edge, SlowDownEdge::Ended { .. }))
            .collect();
        assert_eq!(
            ended,
            [SlowDownEdge::Ended {
                since: 0,
                duration_ms: MINUTE,
                factor_min: 0.6,
                held_ms: 0,
            }],
            "the span ends at the return, not at the last read"
        );
    }

    #[test]
    fn a_span_the_purge_drops_while_the_factor_reads_under_one_ends_at_the_last_read() {
        let mut state = RoutingState::from_load(RoutingLoad::default());
        for (at, factor) in [(0, 0.6), (30_000, 0.5)] {
            state.write_slow_down("p", "a", &read_of(factor), 1.0, at, 1_800_000);
        }
        state.purge_slow_downs(&no_pools(), 40_000);
        let ended: Vec<SlowDownEdge> = state
            .take_slow_down_edges(40_000)
            .into_iter()
            .map(|(_, _, edge)| edge)
            .filter(|edge| matches!(edge, SlowDownEdge::Ended { .. }))
            .collect();
        assert_eq!(
            ended,
            [SlowDownEdge::Ended {
                since: 0,
                duration_ms: 30_000,
                factor_min: 0.5,
                held_ms: 0,
            }],
            "the factor never returned, so the span ends at the entry's last read"
        );
    }

    /// `a`'s entry on `p` as a read naming `reset` writes it at `read_at`.
    fn written(state: &mut RoutingState, reset: Option<i64>, read_at: i64) -> SlowDownState {
        state
            .write_slow_down(
                "p",
                "a",
                &SlowDownRead {
                    reset,
                    ..read_of(0.5)
                },
                1.0,
                read_at,
                1_800_000,
            )
            .clone()
    }

    #[test]
    fn slow_down_stale_reads_the_passed_reset_of_the_entrys_window() {
        const MINUTE: i64 = 60_000;
        const R: i64 = 10 * HOUR_MS;
        let fresh = || RoutingState::from_load(RoutingLoad::default());

        let mut state = fresh();
        read(&mut state, FIVE_HOUR, 0.5, Some(R), R - HOUR_MS);
        let entry = written(&mut state, Some(R), R - 30 * MINUTE);
        assert_eq!(entry.reset, Some(R));
        assert!(
            !state.slow_down_stale("a", &entry, R - MINUTE),
            "its own reset lies ahead under a reading naming it"
        );
        assert!(
            state.slow_down_stale("a", &entry, R + MINUTE),
            "its own reset passed under the reading still naming it"
        );
        read(
            &mut state,
            FIVE_HOUR,
            0.1,
            Some(R + 5 * HOUR_MS),
            R + 2 * MINUTE,
        );
        assert!(
            state.slow_down_stale("a", &entry, R + 3 * MINUTE),
            "its own reset passed under a reading naming the next one ahead"
        );

        // Read in the gap after the reset, before any reading of the new window.
        let mut state = fresh();
        read(&mut state, FIVE_HOUR, 0.5, Some(R), R - HOUR_MS);
        let entry = written(&mut state, None, R + 10 * MINUTE);
        assert_eq!(entry.reset, None);
        assert!(
            !state.slow_down_stale("a", &entry, R + 11 * MINUTE),
            "read after the passed reset the reading names"
        );
        let next = R + 10 * MINUTE + 5 * HOUR_MS;
        read(
            &mut state,
            FIVE_HOUR,
            0.1,
            Some(next),
            R + 10 * MINUTE + 200,
        );
        assert!(
            !state.slow_down_stale("a", &entry, R + 11 * MINUTE),
            "read before the new window's reset, which lies ahead"
        );
        assert!(
            state.slow_down_stale("a", &entry, next),
            "the reading names a later reset that has passed"
        );

        // A head's whole seconds and a poll's milliseconds spell one reset half a second apart: whichever has passed
        // answers.
        let mut state = fresh();
        read(&mut state, FIVE_HOUR, 0.5, Some(R), R - HOUR_MS);
        let entry = written(&mut state, Some(R + 500), R - MINUTE);
        assert_eq!(entry.reset, Some(R + 500));
        assert!(
            state.slow_down_stale("a", &entry, R + 200),
            "the reading's spelling has passed"
        );
        assert!(
            state.slow_down_stale("a", &entry, R + 1_000),
            "both spellings have passed"
        );
        let mut state = fresh();
        read(&mut state, FIVE_HOUR, 0.5, Some(R + 500), R - HOUR_MS);
        let entry = written(&mut state, Some(R), R - MINUTE);
        assert_eq!(entry.reset, Some(R));
        assert!(
            state.slow_down_stale("a", &entry, R + 200),
            "the entry's own spelling has passed"
        );
    }

    #[test]
    fn slow_down_stale_reads_the_entrys_own_reset_where_the_reading_names_none() {
        const MINUTE: i64 = 60_000;
        const R: i64 = 10 * HOUR_MS;
        let fresh = || RoutingState::from_load(RoutingLoad::default());

        // A poll of an idle window names no reset, and its reading replaces the one that named the passed reset.
        let mut state = fresh();
        read(&mut state, FIVE_HOUR, 0.5, Some(R), R - HOUR_MS);
        let entry = written(&mut state, Some(R), R - 30 * MINUTE);
        read(&mut state, FIVE_HOUR, 0.0, None, R + MINUTE);
        assert!(
            state.slow_down_stale("a", &entry, R + 2 * MINUTE),
            "its own reset passed under a reading naming none"
        );

        let mut state = fresh();
        let entry = written(&mut state, Some(R), R - 30 * MINUTE);
        assert!(
            !state.slow_down_stale("a", &entry, R - MINUTE),
            "no reading, and its own reset lies ahead"
        );
        assert!(
            state.slow_down_stale("a", &entry, R + MINUTE),
            "no reading, and its own reset passed"
        );

        let mut state = fresh();
        read(&mut state, FIVE_HOUR, 0.0, None, R - HOUR_MS);
        let entry = written(&mut state, None, R - 30 * MINUTE);
        assert!(
            !state.slow_down_stale("a", &entry, R + MINUTE),
            "neither the reading nor the entry names a reset"
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "a realized speed is positive and finite")]
    fn the_engine_never_writes_a_realized_speed_that_is_not_positive() {
        let read = SlowDownRead {
            utilization: 0.5,
            horizon: 3_600_000,
            reset: None,
            demand: 0.0,
            banded: Banded::flat(0.0, 0, 3_600_000),
            stretch_ms: 0,
            threshold_ms: 1_800_000,
            factor: 1.0,
            ramp: 1.0,
            ramped: 1.0,
            speed: 1.0,
            plan: Vec::new(),
            realized: 1.0,
        };
        RoutingState::from_load(RoutingLoad::default()).write_slow_down(
            "p",
            "a",
            &read,
            f64::NAN,
            0,
            1_800_000,
        );
    }
}
