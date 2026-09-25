//! What the router task owns between selections: pins, cooldowns, the newest overage evidence per account, the
//! newest utilization sample per account, and the in-flight counts per key and per account. Every mutation
//! reports whether it changed anything so the task knows what to write through.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard};

use phoebis_store::{
    AccountKind, AccountState, CooldownRow, FitRow, OverageRow, PinRow, RekeyWrite, ResolutionRow,
    RoutingLoad,
};
use tokio::sync::Notify;
use tokio::time::Instant;

use crate::listing::{row_for, snapshot_date};
use crate::RegistrySnapshot;

/// The sample TTL a state nobody configured ages readings on. The router task replaces it with its own
/// configured interval; this matches `RouterConfig::default().sample_ttl`.
const DEFAULT_SAMPLE_TTL_MS: i64 = 60_000;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PinRef {
    pub key_id: String,
    pub session_id: String,
    pub model: String,
    pub epoch: u64,
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

pub const FIVE_HOUR: &str = "five_hour";
pub const SEVEN_DAY: &str = "seven_day";
pub const SEVEN_DAY_FABLE: &str = "seven_day_fable";

/// One window's reading: utilization as a fraction, the reset that refunds it when the source named one, and
/// when it was read; a reading is replaced only by a newer one for the same window.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowReading {
    pub utilization: f64,
    pub reset: Option<i64>,
    pub observed_at: i64,
}

impl WindowReading {
    /// Whether the reading still describes the window at `now`: its reset is ahead, or it names none and was
    /// read within `ttl_ms`. A reading past its reset is refunded; one that aged out says nothing.
    pub fn live_at(&self, now: i64, ttl_ms: i64) -> bool {
        match self.reset {
            Some(reset) => reset > now,
            None => now - self.observed_at < ttl_ms,
        }
    }
}

/// An account's latest readings by window name: `five_hour`, `seven_day`, `seven_day_fable` for a subscription
/// account; `rpm:<model>`, `itpm:<model>`, `otpm:<model>` per canonical model for an api_key account.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct UtilizationSample {
    pub windows: BTreeMap<String, WindowReading>,
    pub observed_at: i64,
}

impl UtilizationSample {
    /// The stored reading, live or not; the callers that rank or gate pass it through `live_at`.
    pub fn reading(&self, window: &str) -> Option<WindowReading> {
        self.windows.get(window).copied()
    }

    /// The worst of the api_key buckets still live at `now`, the account's reading under Pace's api_key
    /// degradation; `None` when every bucket was refunded or aged out, which reads as untouched.
    pub fn worst_bucket(&self, now: i64, ttl_ms: i64) -> Option<WindowReading> {
        self.windows
            .iter()
            .filter(|(name, _)| {
                name.starts_with("rpm:") || name.starts_with("itpm:") || name.starts_with("otpm:")
            })
            .filter_map(|(_, reading)| reading.live_at(now, ttl_ms).then_some(*reading))
            .max_by(|a, b| a.utilization.total_cmp(&b.utilization))
    }

    /// A builder for tests.
    pub fn with(mut self, window: &str, reading: WindowReading) -> UtilizationSample {
        self.windows.insert(window.to_string(), reading);
        self
    }
}

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
    DeleteCooldown(String, String),
    DeletePin(String, String, String),
    /// One resolution landing, committed as one transaction so the store never holds moved rows without the
    /// spelling row that finds them after a restart.
    Rekey(RekeyWrite),
    UpsertCooldown(CooldownRow),
    UpsertFit(FitRow),
    UpsertOverage(OverageRow),
    UpsertPin(PinRow),
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Purged {
    pub cooldowns: Vec<(String, String)>,
    /// Each dropped pin with the one fact that dropped it: `deleted`, `expired`, or `unserved`.
    pub pins: Vec<((String, String, String), &'static str)>,
}

/// A candidate's sort key in `choose`: group, priority, and the overage bit under the kind order; 0, priority,
/// and the overage bit otherwise.
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

/// The price of one token of each type.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Rates {
    pub input: f64,
    pub cache_read: f64,
    pub write_5m: f64,
    pub write_1h: f64,
    pub output: f64,
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
                input: 1.0,
                cache_read: 1.0,
                write_5m: 1.0,
                write_1h: 1.0,
                output: 1.0,
            },
        }
    }

    /// Each count times its rate, summed, from the row `listing::row_for` finds for `canonical_model`, else
    /// from `fallback`.
    pub fn price(&self, canonical_model: &str, usage: PinUsage) -> f64 {
        let rates = row_for(canonical_model, |k| self.rows.get(k)).unwrap_or(&self.fallback);
        usage.input as f64 * rates.input
            + usage.cache_read as f64 * rates.cache_read
            + usage.write_5m as f64 * rates.write_5m
            + usage.write_1h as f64 * rates.write_1h
            + usage.output as f64 * rates.output
    }
}

/// Which of a pin's two averages a reader takes: meter tokens while the account is funded by its plan, dollars
/// on an api_key account or a subscription account in overage. Serializes as `meter_tokens` or `usd`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BurnUnit {
    MeterTokens,
    Usd,
}

impl BurnUnit {
    pub fn of(kind: AccountKind, in_overage: bool) -> BurnUnit {
        match (kind, in_overage) {
            (AccountKind::Oauth, false) => BurnUnit::MeterTokens,
            (AccountKind::Oauth, true) | (AccountKind::ApiKey, _) => BurnUnit::Usd,
        }
    }

    /// `pin.burn` or `pin.burn_usd`.
    pub fn read(self, pin: &PinRow) -> f64 {
        match self {
            BurnUnit::MeterTokens => pin.burn,
            BurnUnit::Usd => pin.burn_usd,
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

/// The fold a renewal with usage applies at `now`: the average decayed to `now`, plus `priced` spread over the
/// window, so the average is priced usage per second.
pub(crate) fn burn_folded(
    burn: f64,
    burn_at: Option<i64>,
    now: i64,
    window_ms: i64,
    priced: f64,
) -> f64 {
    burn_decayed(burn, burn_at, now, window_ms) + priced / (window_ms.max(1) as f64 / 1_000.0)
}

/// Whether the pin's cache entry is still alive at `now`: its last refresh is younger than the TTL in force
/// for it.
pub fn warm(pin: &PinRow, now: i64) -> bool {
    now - pin.last_refresh_at < pin.cache_ttl_ms
}

/// The pins a round marks, in the round's order: never the last unmarked pin, nothing for a zero target.
pub fn shed_marks(ordered: &[(PinRow, f64)], target: f64) -> Vec<(String, String, String)> {
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
        marks.push((
            pin.key_id.clone(),
            pin.session_id.clone(),
            pin.model.clone(),
        ));
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
/// handle and the task. A key permit is one per request, reserved before selection and held across every
/// attempt; an account ticket is one per wire attempt, a leg's first taken by the selection that chose the
/// account and each later one at its dial. Each is released exactly once, by its guard's drop, when the
/// response it counts ends.
#[derive(Clone, Default)]
pub struct InFlight(Arc<InFlightShared>);

impl InFlight {
    /// Check and increment under the one lock: the permit, or `None` when `cap` is reached (`None` is no cap).
    pub fn try_acquire_key(&self, key_id: &str, cap: Option<u32>, now: i64) -> Option<KeyPermit> {
        let mut table = self.0.table();
        let held = table.keys.get(key_id).map(HashMap::len).unwrap_or(0) as u32;
        if cap.is_some_and(|cap| held >= cap) {
            return None;
        }
        let id = table.next_id;
        table.next_id += 1;
        table
            .keys
            .entry(key_id.to_string())
            .or_default()
            .insert(id, now);
        Some(KeyPermit {
            shared: self.0.clone(),
            key_id: key_id.to_string(),
            id,
        })
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

/// When a shedding round ran, and the pins it marked by `(key_id, session_id, model)`.
type ShedRound = (i64, HashSet<(String, String, String)>);

pub struct RoutingState {
    cooldowns: HashMap<(String, String), CooldownRow>,
    /// The ledger's live fit per account and window, kept here because the routing writer persists it.
    fits: HashMap<(String, String), FitRow>,
    in_flight: InFlight,
    next_epoch: u64,
    overage: HashMap<String, OverageEvidence>,
    pins: HashMap<(String, String, String), PinRow>,
    /// A folded spelling to the dated id it resolved to, so every lookup below can key a model's rows on the
    /// dated id once it is known, whichever spelling the caller names.
    resolutions: HashMap<String, String>,
    round_robin: HashMap<(String, Tier, String), usize>,
    sample_ttl_ms: i64,
    samples: HashMap<String, UtilizationSample>,
    /// Each account's latest shedding round. Memory only, so a restart forgets the marks with the dwell and the
    /// next hot request may start a fresh round.
    shed_rounds: HashMap<String, ShedRound>,
}

impl RoutingState {
    pub fn from_load(load: RoutingLoad) -> RoutingState {
        let next_epoch = load.pins.iter().map(|p| p.epoch).max().unwrap_or(0) + 1;
        RoutingState {
            cooldowns: load
                .cooldowns
                .into_iter()
                .map(|c| ((c.account_id.clone(), c.scope.clone()), c))
                .collect(),
            // The row the ledger's fitter refuses on load is refused here too, so no reader divides by it.
            fits: load
                .fits
                .into_iter()
                .filter(FitRow::sound)
                .map(|f| ((f.account_id.clone(), f.window.clone()), f))
                .collect(),
            in_flight: InFlight::default(),
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
            pins: load
                .pins
                .into_iter()
                .map(|p| ((p.key_id.clone(), p.session_id.clone(), p.model.clone()), p))
                .collect(),
            resolutions: load
                .resolutions
                .into_iter()
                .map(|r| (r.spelling, r.dated_id))
                .collect(),
            round_robin: HashMap::new(),
            sample_ttl_ms: DEFAULT_SAMPLE_TTL_MS,
            samples: HashMap::new(),
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

    pub fn pin(&self, key_id: &str, session_id: &str, model: &str) -> Option<&PinRow> {
        let model = self.canonical(model);
        self.pins.get(&(
            key_id.to_string(),
            session_id.to_string(),
            model.to_string(),
        ))
    }

    /// A fresh pin under a key another pin held (a release that lands the session elsewhere) takes the old one
    /// off its account, so it spends that pin's mark as a removal does; a renewal keeps the epoch and the mark.
    pub(crate) fn put_pin(&mut self, mut pin: PinRow) {
        pin.model = self.canonical(&pin.model).to_string();
        let key = (
            pin.key_id.clone(),
            pin.session_id.clone(),
            pin.model.clone(),
        );
        let epoch = pin.epoch;
        if let Some(replaced) = self.pins.insert(key, pin) {
            if replaced.epoch != epoch {
                self.spend_mark(&replaced);
            }
        }
    }

    pub(crate) fn remove_pin(
        &mut self,
        key_id: &str,
        session_id: &str,
        model: &str,
    ) -> Option<PinRow> {
        let model = self.canonical(model).to_string();
        self.take_pin(&(key_id.to_string(), session_id.to_string(), model))
    }

    /// The one way a pin leaves the map: a pin that left its account, by a move, a fail-over, an expiry, a
    /// release or the purge, spends its mark there, so coming back inside the dwell does not shed it on a round
    /// that marked it before it left.
    fn take_pin(&mut self, key: &(String, String, String)) -> Option<PinRow> {
        let pin = self.pins.remove(key)?;
        self.spend_mark(&pin);
        Some(pin)
    }

    fn spend_mark(&mut self, pin: &PinRow) {
        if let Some((_, marks)) = self.shed_rounds.get_mut(&pin.account_id) {
            marks.remove(&(
                pin.key_id.clone(),
                pin.session_id.clone(),
                pin.model.clone(),
            ));
        }
    }

    /// The next index into `cohort` for a pool's tier under round robin. The key carries the cohort because
    /// `resolve` builds a candidate list per model: a singleton model and a three-account model share a pool and
    /// a tier but rotate apart, and a cohort whose membership changed is a key of its own that starts at 0. An
    /// account id is a slug, so joining the sorted ids names one cohort and no other; the caller sorts, and an
    /// unsorted cohort would split one rotation into several. In memory, so a restart starts every round again.
    pub(crate) fn round_robin_next(&mut self, pool_id: &str, tier: Tier, cohort: &[&str]) -> usize {
        let key = (pool_id.to_string(), tier, cohort.join(","));
        let slot = self.round_robin.entry(key).or_insert(0);
        let index = *slot % cohort.len();
        *slot = index + 1;
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

    /// The live rate-limit rows on the account that `sample` shows recovered: the window the row names has
    /// allowance again, read from a sample taken after the rejection that set the row. A window the sample
    /// never read is not evidence, and neither is a row no usage window answers for. A row already past its
    /// deadline is left to the purge tick, which says so. Only an OAuth account is ever sampled, which is what
    /// keeps an api-key account's rows out of here without a registry lookup.
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
            if row.scope.starts_with("model:") {
                return below(SEVEN_DAY_FABLE);
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
        let canonical_model = self.canonical(canonical_model);
        let account = self
            .cooldowns
            .get(&(account_id.to_string(), "account".to_string()))
            .filter(|c| c.until > now);
        let model = self
            .cooldowns
            .get(&(account_id.to_string(), format!("model:{canonical_model}")))
            .filter(|c| c.until > now);
        match (account, model) {
            (Some(a), Some(m)) => Some(if a.until >= m.until { a } else { m }),
            (a, m) => a.or(m),
        }
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

    /// Folds `sample` in window by window; true when any reading moved. A reading lands only when it is newer
    /// than the one already stored for that name, so an api_key head that carries one model's buckets never
    /// erases another model's.
    pub fn record_sample(&mut self, account_id: &str, sample: UtilizationSample) -> bool {
        if sample.windows.is_empty() {
            return false;
        }
        let current = self.samples.entry(account_id.to_string()).or_default();
        let mut changed = false;
        for (window, reading) in sample.windows {
            let newer = current
                .windows
                .get(&window)
                .is_none_or(|existing| reading.observed_at > existing.observed_at);
            if newer {
                current.windows.insert(window, reading);
                changed = true;
            }
        }
        if changed {
            current.observed_at = current
                .windows
                .values()
                .map(|r| r.observed_at)
                .max()
                .unwrap_or(0);
        }
        changed
    }

    pub fn sample(&self, account_id: &str) -> Option<&UtilizationSample> {
        self.samples.get(account_id)
    }

    pub(crate) fn forget_sample(&mut self, account_id: &str) {
        self.samples.remove(account_id);
    }

    /// Keeps the row unless the one held for its account and window has folded later; a row written for the
    /// carry alone keeps its fold's `fitted_at`, so an equal time replaces too.
    pub fn record_fit(&mut self, row: FitRow) -> bool {
        let key = (row.account_id.clone(), row.window.clone());
        if self
            .fits
            .get(&key)
            .is_some_and(|held| held.fitted_at > row.fitted_at)
        {
            return false;
        }
        self.fits.insert(key, row);
        true
    }

    /// Every account's fit rows, by account and window.
    pub fn fits(&self) -> Vec<FitRow> {
        let mut out: Vec<FitRow> = self.fits.values().cloned().collect();
        out.sort_by(|a, b| (&a.account_id, &a.window).cmp(&(&b.account_id, &b.window)));
        out
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

    /// How long a reading stands when it dates no refund of its own: the interval the router pulls samples
    /// on, so a window read full is re-read rather than believed for ever.
    pub fn sample_ttl_ms(&self) -> i64 {
        self.sample_ttl_ms
    }

    pub(crate) fn set_sample_ttl_ms(&mut self, ms: i64) {
        self.sample_ttl_ms = ms;
    }

    /// The shared in-flight table this state's selections read the cap against: the same one the handle a test
    /// dispatches through would hold a ticket on, so a test can seed the cap without a handle.
    pub fn in_flight(&self) -> &InFlight {
        &self.in_flight
    }

    pub(crate) fn set_in_flight(&mut self, in_flight: InFlight) {
        self.in_flight = in_flight;
    }

    /// The pins of an account, the burn of each in `unit` decayed to `now`, and the sum.
    pub fn burn_of(
        &self,
        account_id: &str,
        unit: BurnUnit,
        now: i64,
        window_ms: i64,
    ) -> (Vec<(PinRow, f64)>, f64) {
        let mut pins: Vec<(PinRow, f64)> = self
            .pins
            .values()
            .filter(|p| p.account_id == account_id)
            .map(|p| {
                let burn = burn_decayed(unit.read(p), p.burn_at, now, window_ms);
                (p.clone(), burn)
            })
            .collect();
        pins.sort_by(|a, b| {
            (&a.0.key_id, &a.0.session_id, &a.0.model).cmp(&(
                &b.0.key_id,
                &b.0.session_id,
                &b.0.model,
            ))
        });
        let sum = pins.iter().map(|(_, burn)| burn).sum();
        (pins, sum)
    }

    /// Whether the latest round on the pin's current account, run within `dwell_ms`, marked it. A pin marked
    /// on one account and moved keeps its key, so the round read is the one of the account it now sits on.
    pub fn shed_marked(
        &self,
        key_id: &str,
        session_id: &str,
        model: &str,
        now: i64,
        dwell_ms: i64,
    ) -> bool {
        let model = self.canonical(model);
        let Some(pin) = self.pin(key_id, session_id, model) else {
            return false;
        };
        self.shed_rounds
            .get(&pin.account_id)
            .is_some_and(|(at, marks)| {
                now - at < dwell_ms
                    && marks.contains(&(
                        key_id.to_string(),
                        session_id.to_string(),
                        model.to_string(),
                    ))
            })
    }

    /// When the account's latest shedding round ran.
    pub fn last_shed(&self, account_id: &str) -> Option<i64> {
        self.shed_rounds.get(account_id).map(|(at, _)| *at)
    }

    pub(crate) fn record_shed(
        &mut self,
        account_id: &str,
        at: i64,
        marks: Vec<(String, String, String)>,
    ) {
        self.shed_rounds
            .insert(account_id.to_string(), (at, marks.into_iter().collect()));
    }

    /// A late failure from account A must not remove a newer pin on account B, so both have to match.
    pub fn invalidate_pin(&mut self, pin: &PinRef, account_id: &str) -> bool {
        let key = (
            pin.key_id.clone(),
            pin.session_id.clone(),
            self.canonical(&pin.model).to_string(),
        );
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

        let moved_pins: Vec<PinRow> = self
            .pins
            .values()
            .filter(|p| p.model == spelling)
            .cloned()
            .collect();
        for pin in moved_pins {
            self.take_pin(&(
                pin.key_id.clone(),
                pin.session_id.clone(),
                pin.model.clone(),
            ));
            rekey.delete_pins.push((
                pin.key_id.clone(),
                pin.session_id.clone(),
                pin.model.clone(),
            ));
            let dated_key = (
                pin.key_id.clone(),
                pin.session_id.clone(),
                dated_id.to_string(),
            );
            let keep_existing = self
                .pins
                .get(&dated_key)
                .is_some_and(|existing| existing.last_used_at >= pin.last_used_at);
            if !keep_existing {
                let moved = PinRow {
                    model: dated_id.to_string(),
                    ..pin
                };
                if let Some(replaced) = self.pins.insert(dated_key, moved.clone()) {
                    self.spend_mark(&replaced);
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

    pub fn release_session(
        &mut self,
        key_id: &str,
        session_id: &str,
    ) -> Vec<(String, String, String)> {
        let gone: Vec<_> = self
            .pins
            .keys()
            .filter(|(k, s, _)| k == key_id && s == session_id)
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
            (&a.key_id, &a.session_id, &a.model).cmp(&(&b.key_id, &b.session_id, &b.model))
        });
        v
    }

    /// Expired cooldowns, pins past their pool's idle TTL, pins whose key or pool is gone, and everything
    /// held against an account the registry no longer has. An id is reusable, so state the deleted account
    /// left behind would otherwise be inherited by whatever is created under that id next: a two-day cooldown
    /// would keep a brand new account out of every pool it joins. A delete and a create under one id can both
    /// land before one reload, so a row is held against the account only while the snapshot's holder carries
    /// the incarnation it names. A pin names its key's creation time too, and one whose key or account was
    /// replaced reads `deleted`: its holder is gone, whatever took the id.
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
                    .map(|pool| {
                        i64::try_from(pool.affinity_ttl_seconds)
                            .ok()
                            .and_then(|s| s.checked_mul(1_000))
                            .unwrap_or(i64::MAX)
                    });
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
        // None of these needs a deletion written: the overage and fit rows cascade with the account and the next
        // holder's first reading replaces one left under another incarnation, and samples, round-robin cursors
        // and shedding rounds are held in memory only. Samples and cursors are keyed by id alone, so the reload
        // drops the sample of a holder the id has moved past.
        let absent = |account_id: &str| snapshot.account(account_id).is_none();
        self.overage
            .retain(|account_id, evidence| !gone(account_id, evidence.incarnation));
        self.fits
            .retain(|(account_id, _), fit| !gone(account_id, fit.incarnation));
        self.samples.retain(|account_id, _| !absent(account_id));
        self.round_robin.retain(|(pool_id, _, cohort), _| {
            snapshot.pool(pool_id).is_some() && cohort.split(',').all(|id| !absent(id))
        });
        // A round is read only within its pool's dwell, so one older than every pool's has nothing left to say.
        let longest_dwell = snapshot
            .pools()
            .map(|pool| {
                i64::try_from(pool.shedding.source_dwell_seconds)
                    .ok()
                    .and_then(|s| s.checked_mul(1_000))
                    .unwrap_or(i64::MAX)
            })
            .max()
            .unwrap_or(0);
        self.shed_rounds
            .retain(|_, (at, _)| now - *at < longest_dwell);
        Purged { cooldowns, pins }
    }
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

    fn burning(session: &str, burn: f64) -> (PinRow, f64) {
        let pin = PinRow {
            key_id: "k".into(),
            session_id: session.into(),
            model: "m".into(),
            account_id: "a".into(),
            epoch: 1,
            created_at: 0,
            last_used_at: 0,
            key_generation: 0,
            incarnation: 1,
            burn,
            burn_at: None,
            burn_usd: burn,
            last_input_tokens: 0,
            last_refresh_at: 0,
            cache_ttl_ms: 300_000,
            shed_from: None,
            shed_at: None,
            static_hash: None,
            tail_hash: None,
        };
        (pin, burn)
    }

    fn sessions(marks: &[(String, String, String)]) -> Vec<&str> {
        marks.iter().map(|(_, s, _)| s.as_str()).collect()
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
    fn a_dated_model_id_prices_at_its_undated_row_and_an_unlisted_one_at_the_fallback() {
        let flat = |r: f64| Rates {
            input: r,
            cache_read: r,
            write_5m: r,
            write_1h: r,
            output: r,
        };
        let table = RateTable {
            rows: [
                (
                    "claude-sonnet-4-5".to_string(),
                    Rates {
                        input: 3.0,
                        cache_read: 0.25,
                        write_5m: 3.75,
                        write_1h: 6.0,
                        output: 15.0,
                    },
                ),
                (
                    "claude-fable-5-1".to_string(),
                    Rates {
                        input: 5.0,
                        cache_read: 0.5,
                        write_5m: 6.25,
                        write_1h: 10.0,
                        output: 25.0,
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
            30.0 + 25.0 + 15.0 + 12.0 + 45.0,
            "the dated id prices at the undated sonnet row"
        );
        assert_eq!(
            table.price("claude-fable-5-1-20260601", usage),
            50.0 + 50.0 + 25.0 + 20.0 + 75.0,
            "and the dated fable id at its own row, not the fallback"
        );
        assert_eq!(
            table.price("claude-opus-5-20260101", usage),
            10.0 + 100.0 + 4.0 + 2.0 + 3.0,
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

        let changed = state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 1_000,
                ..Default::default()
            }
            .with("itpm:x", reading(0.9, 1_000)),
        );
        assert!(!changed, "1_000 is older than the 2_000 x already carries");
        assert_eq!(
            state
                .sample("a")
                .unwrap()
                .reading("itpm:x")
                .map(|r| r.utilization),
            Some(0.4)
        );

        let changed = state.record_sample(
            "a",
            UtilizationSample {
                observed_at: 3_000,
                ..Default::default()
            }
            .with("itpm:x", reading(0.1, 3_000)),
        );
        assert!(changed);
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
    fn a_bucket_past_its_reset_is_refunded_and_a_reading_without_a_reset_ages_out_after_the_sample_ttl(
    ) {
        let ttl_ms = 1_000;
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
            sample.worst_bucket(1_300, ttl_ms).map(|r| r.utilization),
            Some(0.9),
            "x is live and worse than y before its reset"
        );
        assert_eq!(
            sample.worst_bucket(1_500, ttl_ms).map(|r| r.utilization),
            Some(0.3),
            "past x's reset it is refunded, so y answers as the worst live bucket"
        );
        assert_eq!(
            sample.worst_bucket(1_000 + ttl_ms + 1, ttl_ms),
            None,
            "y names no reset and ages out ttl_ms after it was observed, leaving nothing live"
        );
    }
}
