//! Quota samples from two sources: every response head of an OAuth account, and a poll of the usage endpoint
//! every five minutes per account. Heads cost nothing and cover active accounts; the poll fills idle gaps and is
//! the only source of extra-usage credit totals.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_oauth::{
    BackoffEdge, LandedMark, PollFault, PollIssue, ProfileResponse, UsageError, UsageService,
    UsageSnapshot,
};
use phoebis_relay::UnifiedHeaders;
use phoebis_routing::{
    credits_from_usage, elapsed_share_at, sample_from_usage, Report, RouterHandle, FIVE_HOUR,
    SEVEN_DAY, SEVEN_DAY_FABLE, SEVEN_DAY_OVERAGE_INCLUDED,
};
use phoebis_store::{AccountKind, AccountRow, AccountState, Store};
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::assemble::Record;
use crate::events::{plan_ended, plan_restored, poll_backoff, poll_failed};
use crate::plan::PlanTables;
use crate::record::{ExtraUsageSampleRecord, QuotaSampleRecord, SampleSource};
use crate::rollup::bucket_of;
use crate::task::Ledger;

/// How often every polled OAuth account's usage endpoint is read: often enough that a routing decision lags use of
/// the account outside the router by about this, longer while the endpoint rate-limits the poll, rare enough that
/// the poll alone never reaches the endpoint's per-account limit; the first live run moves it to ten minutes if the
/// 429 warnings say so.
pub const POLL_INTERVAL: Duration = Duration::from_secs(300);

/// How much sooner than an interval after a read the next slot may come: the loop wakes a little after a slot is
/// due, so an on-time read lands just past its slot, and without this slack the next slot on its phase would be
/// too soon and the read would skip to the slot after next.
const WAKE_TOLERANCE: Duration = Duration::from_secs(1);

/// The profile is read again on every third poll of an account, a quarter hour apart. It is not read on every poll
/// because the profile endpoint shares the usage endpoint's per-account limit and a profile 429 pauses the account's
/// usage poll for up to fifteen minutes. A faster cadence waits for a live measure of that limit.
pub const PROFILE_EVERY_POLLS: u32 = 3;

const OPEN_RUNS_POISONED: &str = "the poll backoff's open-run set poisoned by a prior panic";

const WARNED_TIERS_POISONED: &str = "the warned tiers set poisoned by a prior panic";

/// The accounts and tier words a Team or Enterprise warn already named, so each pair logs once. The process's, since
/// the reads that consult it run on the poll's tasks and on the admin's refresh, and a restart clears it.
static WARNED_TIERS: Mutex<BTreeSet<(String, String)>> = Mutex::new(BTreeSet::new());

/// The `source` of a plan event a profile read wrote, the third poll's, the startup's or the Refresh button's.
const PROFILE_SOURCE: &str = "profile";

/// The `source` of a `plan_ended` row whose profile read a usage 403 made due.
const USAGE_403_SOURCE: &str = "usage_403";

/// What a profile read found about the plan, which decides whether the usage 403 that made it due is a passing fault.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ProfileRead {
    NoPlan,
    Plan,
    Unread,
}

/// Whether the profile document names an organization with no plan, the type an ended subscription's profile reads.
fn reports_no_plan(profile: &serde_json::Value) -> bool {
    profile["organization"]["organization_type"] == "claude_free"
}

/// One warning per account and tier word for a stored Team or Enterprise document whose tier the cost model in force
/// holds no capacity row for, so an operator pairing such a seat sees why it reads unpriced.
fn warn_unpriced_tier(tables: &PlanTables, at: i64, account_id: &str, plan: &str, tier: &str) {
    if !matches!(plan, "team" | "enterprise")
        || tables
            .revisions
            .in_force(at)
            .capacity_of(None, tier)
            .is_some()
    {
        return;
    }
    let first = WARNED_TIERS
        .lock()
        .expect(WARNED_TIERS_POISONED)
        .insert((account_id.to_string(), tier.to_string()));
    if first {
        tracing::warn!(
            account = %account_id,
            plan = %plan,
            rate_limit_tier = %tier,
            "the cost model holds no capacity row for the seat's tier, so the seat reads unpriced"
        );
    }
}

/// The assembler's entry: it parsed the head once for the attempt row and hands that parse here rather than
/// paying for a second clone and parse on every response head.
pub(crate) fn samples_from_unified(
    account_id: &str,
    unified: &UnifiedHeaders,
    observed_at: i64,
    usage_based: bool,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let bucket = bucket_of(observed_at);
    let mut quota = Vec::new();
    // The head names its windows `5h`, `7d` and `7d_oi`; the table spells them by the usage endpoint's keys, so a
    // head and a poll of one bucket write one row per window.
    let windows = [
        (FIVE_HOUR, &unified.five_hour),
        (SEVEN_DAY, &unified.seven_day),
        (SEVEN_DAY_OVERAGE_INCLUDED, &unified.seven_day_oi),
    ];
    for (window, reading) in windows {
        if reading.utilization.is_some() || reading.reset.is_some() {
            quota.push(QuotaSampleRecord {
                account_id: account_id.into(),
                bucket,
                window: window.into(),
                utilization: reading.utilization,
                reset_at: reading.reset,
                source: SampleSource::Head,
                observed_at,
            });
        }
    }
    let overage_utilization = unified.overage_utilization;
    // A usage-based seat's overage family is its spend window, never an overage episode, so its row keeps the share
    // and the reset with no in-use flag, written whenever the head moves either.
    let in_use = if usage_based {
        (overage_utilization.is_some() || unified.overage_reset.is_some()).then_some(None)
    } else {
        unified.overage_in_use.map(Some)
    };
    let extra = in_use.map(|in_use| ExtraUsageSampleRecord {
        account_id: account_id.into(),
        bucket,
        is_enabled: true,
        in_use,
        monthly_limit: None,
        used_credits: None,
        utilization: overage_utilization,
        currency: None,
        disabled_reason: unified.overage_disabled_reason.clone(),
        overage_reset: unified.overage_reset,
        source: SampleSource::Head,
        spend_limit_minor: None,
        spend_used_minor: None,
        spend_percent: None,
        spend_currency: None,
        spend_exponent: None,
    });
    (quota, extra)
}

pub fn samples_from_poll(
    snapshot: &UsageSnapshot,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let bucket = bucket_of(snapshot.fetched_at);
    // The endpoint reports percent and the headers a fraction, so the table holds one scale, the extra-usage row's
    // share included.
    let row = |key: &str, utilization: Option<f64>, reset_at: Option<i64>| QuotaSampleRecord {
        account_id: snapshot.account_id.clone(),
        bucket,
        window: key.into(),
        utilization: utilization.map(|u| u / 100.0),
        reset_at,
        source: SampleSource::Poll,
        observed_at: snapshot.fetched_at,
    };
    // The `limits` window is the Fable week, as the router reads it: a top-level key of the same name would be a
    // second reading of one window, and the table keeps one row per window.
    let mut quota: Vec<QuotaSampleRecord> = snapshot
        .windows
        .iter()
        .filter(|w| snapshot.fable_window.is_none() || w.key != SEVEN_DAY_FABLE)
        .map(|w| row(&w.key, w.utilization, w.resets_at))
        .collect();
    if let Some(fable) = &snapshot.fable_window {
        quota.push(row(SEVEN_DAY_FABLE, fable.utilization, fable.resets_at));
    }
    // The `spend` columns hold the body's object as it gave it, so a reading the extra-usage amounts built leaves
    // them null and the view falls back to the amounts beside them.
    let spend = snapshot.spend.as_ref().filter(|s| s.from_spend);
    let extra = snapshot
        .extra_usage
        .as_ref()
        .map(|e| ExtraUsageSampleRecord {
            account_id: snapshot.account_id.clone(),
            bucket,
            is_enabled: e.is_enabled,
            in_use: None,
            monthly_limit: e.monthly_limit,
            used_credits: e.used_credits,
            utilization: e.utilization.map(|u| u / 100.0),
            currency: e.currency.clone(),
            disabled_reason: e.disabled_reason.clone(),
            overage_reset: None,
            source: SampleSource::Poll,
            spend_limit_minor: spend.and_then(|s| s.limit_minor),
            spend_used_minor: spend.map(|s| s.used_minor),
            spend_percent: spend.and_then(|s| s.percent),
            spend_currency: spend.map(|s| s.currency.clone()),
            spend_exponent: spend.map(|s| s.exponent),
        });
    (quota, extra)
}

/// How far a sampled reading runs ahead of its window's pace at the sample's instant: the utilization less the
/// share of the window elapsed toward the earlier of its reset and the account's `end`, the router's own arithmetic
/// (`elapsed_share_at`) over the raw sample rather than its prediction.
pub fn lead_over_pace(
    window: &str,
    utilization: f64,
    reset_at: Option<i64>,
    end: Option<i64>,
    at: i64,
) -> f64 {
    utilization - elapsed_share_at(window, reset_at, end, at)
}

/// Whether the poll asks about this account: every OAuth account that holds a credential the upstream has not
/// refused, paused or not, because a paused account's quota is still the operator's to watch. A refused credential
/// is asked again only after a re-pair.
pub fn polled(kind: AccountKind, state: AccountState) -> bool {
    kind == AccountKind::Oauth
        && !matches!(
            state,
            AccountState::Disconnected | AccountState::Unpaired | AccountState::ReauthRequired
        )
}

/// What the startup step found before the bound: `reported` accounts reached the router with a sample,
/// `failed` answered no clean snapshot, `pending` had reported nothing when the bound passed, and `paused` stand
/// flagged plan ended, so the step read their profile alone and asked no usage of them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sampled {
    pub reported: usize,
    pub failed: usize,
    pub pending: usize,
    pub paused: usize,
}

/// One polled account's place in the schedule.
struct Slot {
    /// The row the attempt is made with, kept so a registry read that fails leaves the schedule as it was. Its
    /// incarnation is the one the slot was drawn for: a re-pairing is a new account to the schedule, and a completion
    /// drawn for another incarnation moves nothing here.
    account: AccountRow,
    /// The attempt out on this slot, by its task id; `None` between attempts.
    in_flight: Option<tokio::task::Id>,
    /// The next attempt's instant, or the slot the attempt in flight was drawn at.
    at: Instant,
    /// Reads counted on this slot's incarnation, the startup step's among them for a slot the first pass
    /// drew; an attempt made while the count is a multiple of `PROFILE_EVERY_POLLS` reads the profile, so a
    /// new account's first attempt reads it and a first-pass slot's first attempt does not.
    polls: u32,
    /// When the account was last read: the last attempt's dispatch, or the scheduler's start for a slot the
    /// first pass drew, standing in for the startup step's read. `None` until a new account's first attempt.
    last_read: Option<Instant>,
}

/// What an attempt answers the scheduler with: whose slot, drawn for which incarnation, and the wait the
/// service asks for after it, if any.
struct Completion {
    account_id: String,
    incarnation: u64,
    held: Option<Duration>,
}

/// The first slot on the account's phase, `at` plus whole intervals, that comes no sooner than an interval after
/// the read dispatched at `dispatched`, less `WAKE_TOLERANCE`: an on-time read keeps its phase, and a wake several
/// slots late reads once and skips the slots it missed rather than replaying them or leaving the phase.
fn next_slot(at: Instant, dispatched: Instant) -> Instant {
    let due = dispatched + POLL_INTERVAL - WAKE_TOLERANCE;
    let mut next = at + POLL_INTERVAL;
    while next < due {
        next += POLL_INTERVAL;
    }
    next
}

/// Draws the schedule again when the polled accounts differ from the ones it was drawn for: a pairing, a
/// re-pairing or a delete re-spreads the phases, an attempt in flight keeps its slot, an account is not read
/// again sooner than an interval after its last read, and a held account's slot is the hold's end.
/// The first pass follows the startup step, which has just read every account it lists, so those slots start
/// an interval on with that read counted as their first poll.
fn spread(
    slots: &mut BTreeMap<String, Slot>,
    rows: Vec<AccountRow>,
    usage: &UsageService,
    now: Instant,
    first_pass: bool,
) {
    let accounts: Vec<AccountRow> = rows
        .into_iter()
        .filter(|a| polled(a.kind, a.state))
        .collect();
    let known: BTreeSet<(String, u64)> = accounts
        .iter()
        .map(|a| (a.id.clone(), a.incarnation))
        .collect();
    let drawn: BTreeSet<(String, u64)> = slots
        .iter()
        .map(|(id, slot)| (id.clone(), slot.account.incarnation))
        .collect();
    if drawn == known {
        return;
    }
    slots.retain(|id, slot| known.contains(&(id.clone(), slot.account.incarnation)));
    for (index, account) in accounts.iter().enumerate() {
        let slot = slots.entry(account.id.clone()).or_insert_with(|| Slot {
            account: account.clone(),
            in_flight: None,
            at: now,
            polls: u32::from(first_pass),
            last_read: first_pass.then_some(now),
        });
        slot.account = account.clone();
        if slot.in_flight.is_none() {
            let mut at = now + QuotaPoll::phase(index, accounts.len());
            if first_pass {
                at += POLL_INTERVAL;
            }
            if let Some(read) = slot.last_read {
                at = at.max(read + POLL_INTERVAL);
            }
            // A hold ends when the service says it ends: a 429's wait is the endpoint's own limit per account, and
            // the cadence floor would make a two-minute hold a five-minute one.
            if let Some(held) = usage.held_for(&account.id) {
                at = now + held;
            }
            slot.at = at;
        }
    }
}

pub struct QuotaPoll;

impl QuotaPoll {
    /// The `index`-th of `count` phases spread evenly across `POLL_INTERVAL`, so no two accounts ask in the
    /// same second while fewer than three hundred are polled.
    pub fn phase(index: usize, count: usize) -> Duration {
        let interval_ms = POLL_INTERVAL.as_millis() as u64;
        Duration::from_millis(interval_ms * index as u64 / count.max(1) as u64)
    }

    /// Every account `polled` accepts, each on its own phase within `POLL_INTERVAL` (its position among the
    /// polled accounts sorted by id, re-spread when an account joins or leaves), every attempt spawned into
    /// the scheduler's own set and reaped when it completes, so a slow answer or a backoff on one account
    /// delays no other and no finished attempt is kept; a completion moves its slot only while the slot still
    /// belongs to the incarnation the attempt was drawn for. The next slot is the first on the account's phase an
    /// interval after the read (`next_slot`), so a late wake reads once and keeps the phase, or, after a 429, the
    /// usage service's next-attempt time, which may come sooner than the cadence; with no slot to wait for, the
    /// scheduler wakes after one interval and re-reads the registry. Every clean snapshot is recorded to the ledger
    /// and reported to the router as `poll_account` reports it.
    ///
    /// Spawned once the startup step is done: that step has just read every account, so an account the first
    /// pass finds is next read an interval after the start, on its phase, or when its hold ends after a 429, and
    /// its profile a quarter hour on.
    pub fn spawn(
        store: Arc<Store>,
        usage: Arc<UsageService>,
        ledger: Ledger,
        router: RouterHandle,
        tables: Arc<PlanTables>,
    ) -> tokio::task::JoinHandle<()> {
        usage.set_on_backoff(Arc::new({
            let ledger = ledger.clone();
            // The accounts whose run this hook saw start: the startup step reads every account before this
            // registers, so a run its 429 opened ends here without a start row, and that end is not written. Every
            // start is written, since the service fires one only when a run opens: a run an `invalidate` dropped
            // never ends, and the next one's start still lands.
            let open = Mutex::new(HashSet::new());
            move |edge| {
                let mut open = open.lock().expect(OPEN_RUNS_POISONED);
                let write = match &edge {
                    BackoffEdge::Started { account_id, .. } => {
                        open.insert(account_id.clone());
                        true
                    }
                    BackoffEdge::Ended { account_id, .. } => open.remove(account_id),
                };
                if write {
                    ledger.record(poll_backoff(&edge));
                }
            }
        }));
        tokio::spawn(async move {
            let mut slots: BTreeMap<String, Slot> = BTreeMap::new();
            // The scheduler owns its attempts: a completion is reaped here, and dropping the set when the
            // scheduler is aborted aborts what is still out.
            let mut attempts: JoinSet<Completion> = JoinSet::new();
            let mut first_pass = true;
            let mut registry_failing = false;
            loop {
                let now = Instant::now();
                match store.accounts() {
                    Ok(rows) => {
                        spread(&mut slots, rows, &usage, now, first_pass);
                        first_pass = false;
                        registry_failing = false;
                    }
                    // The slots stand: a registry that cannot be read is no reason to stop polling the
                    // accounts it last listed, and the next pass reads it again. A pass runs on every wake
                    // and every completion, so the outage is logged once, when it starts.
                    Err(error) => {
                        if !registry_failing {
                            tracing::warn!(%error, "the registry could not be read for the poll");
                        }
                        registry_failing = true;
                    }
                }
                for slot in slots.values_mut() {
                    if slot.in_flight.is_some() || slot.at > now {
                        continue;
                    }
                    let profile_due = slot.polls % PROFILE_EVERY_POLLS == 0;
                    slot.polls = slot.polls.wrapping_add(1);
                    slot.last_read = Some(now);
                    let (account, incarnation) = (slot.account.clone(), slot.account.incarnation);
                    let (store, usage, tables, ledger, router) = (
                        store.clone(),
                        usage.clone(),
                        tables.clone(),
                        ledger.clone(),
                        router.clone(),
                    );
                    let handle = attempts.spawn(async move {
                        // A slot of an account whose plan ended keeps its cadence for the third poll's profile read.
                        let mut profile_read = false;
                        if !Self::plan_paused(&store, &account.id) {
                            (_, profile_read) = Self::poll_reading(
                                &store, &usage, &tables, &ledger, &router, &account,
                            )
                            .await;
                        }
                        // The profile endpoint shares the usage endpoint's limit, so a read the poll made serves the
                        // slot's.
                        if profile_due && !profile_read {
                            Self::refresh_profile(
                                &store, &usage, &tables, &ledger, &router, &account,
                            )
                            .await;
                        }
                        Completion {
                            held: usage.held_for(&account.id),
                            account_id: account.id,
                            incarnation,
                        }
                    });
                    slot.in_flight = Some(handle.id());
                }
                // With nothing idle to wait for, one interval: a pairing into an empty schedule is found on the
                // next pass. A backoff longer than that still lets the registry be read once an interval.
                let wake = slots
                    .values()
                    .filter(|slot| slot.in_flight.is_none())
                    .map(|slot| slot.at)
                    .min()
                    .map_or(now + POLL_INTERVAL, |at| at.min(now + POLL_INTERVAL));
                tokio::select! {
                    _ = tokio::time::sleep_until(wake) => {}
                    Some(joined) = attempts.join_next_with_id() => match joined {
                        Ok((id, done)) => {
                            // A completion drawn for a holder the slot no longer belongs to moves nothing: the
                            // replacement's own attempt keeps its own time.
                            if let Some(slot) = slots.get_mut(&done.account_id) {
                                if slot.in_flight == Some(id) && slot.account.incarnation == done.incarnation {
                                    slot.in_flight = None;
                                    let next = next_slot(slot.at, slot.last_read.unwrap_or(slot.at));
                                    slot.at = done.held.map_or(next, |wait| Instant::now() + wait);
                                }
                            }
                        }
                        Err(error) => {
                            tracing::warn!(%error, "a poll attempt ended without completing");
                            if let Some(slot) =
                                slots.values_mut().find(|slot| slot.in_flight == Some(error.id()))
                            {
                                slot.in_flight = None;
                                slot.at = Instant::now() + POLL_INTERVAL;
                            }
                        }
                    },
                }
            }
        })
    }

    /// One account's reading through the service's hold, concurrency cap and coalescing: a clean snapshot is recorded
    /// to the ledger and reported to the router as `Report::Sample` and, when the snapshot carries the evidence,
    /// `Report::Overage` and `Report::Credits`, before this returns. The service's answer is returned as it came, so a
    /// caller that shows it shows what was recorded: a snapshot whose `error` is set (the account held, or the read
    /// failed) was recorded nowhere, and the service's own error is a read that never completed. The scheduler keeps
    /// nothing of the answer, and the startup step counts a clean snapshot as reported. A reauth mark the read's
    /// acquisition landed reaches the router as `Report::AccountState`, as does the mark a usage 401 lands under
    /// `auth_failed`, and any other failed answer or transport failure of the read writes a `poll_failed` row, but for
    /// a usage 403 naming the organization's refusal whose profile, read at once, reports no plan, which writes
    /// `plan_ended` instead. The profile is otherwise `refresh_profile`'s.
    pub async fn poll_account(
        store: &Store,
        usage: &UsageService,
        tables: &PlanTables,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
    ) -> Result<UsageSnapshot, UsageError> {
        Self::poll_reading(store, usage, tables, ledger, router, account)
            .await
            .0
    }

    /// `poll_account` with whether the poll read the profile, which a usage 403 naming the organization's refusal
    /// makes it do, so a caller due to read the profile leaves its own read out.
    async fn poll_reading(
        store: &Store,
        usage: &UsageService,
        tables: &PlanTables,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
    ) -> (Result<UsageSnapshot, UsageError>, bool) {
        let mut profile_read = false;
        let answer = match usage.poll_reporting(&account.id).await {
            Ok((snapshot, issue)) => {
                if let Some(issue) = issue {
                    profile_read =
                        Self::report_issue(store, usage, tables, ledger, router, account, issue)
                            .await;
                }
                Ok(snapshot)
            }
            Err(error) => Err(error),
        };
        Self::report_landed_mark(usage, router, &account.id).await;
        // A snapshot with an error is a stale one, and a stale sample must not be written under a fresh bucket.
        let answer = match answer {
            Ok(snapshot) if snapshot.error.is_none() => {
                let (quota, extra) = samples_from_poll(&snapshot);
                ledger.record(Record::Samples {
                    quota,
                    extra,
                    poll: true,
                });
                // Reported before the profile refresh, so a stalled profile endpoint holds no sample back
                // from selection. Both halves go, as the selection's own pull sends them, so the first
                // selection sees the overage state too.
                let (sample, overage) = sample_from_usage(&snapshot, account.incarnation);
                router
                    .report(Report::Sample {
                        account_id: account.id.clone(),
                        sample,
                        incarnation: account.incarnation,
                        source_request_id: None,
                    })
                    .await;
                if let Some(evidence) = overage {
                    router
                        .report(Report::Overage {
                            account_id: account.id.clone(),
                            evidence,
                            incarnation: account.incarnation,
                        })
                        .await;
                }
                if let Some(state) = credits_from_usage(&snapshot, account.incarnation) {
                    router
                        .report(Report::Credits {
                            account_id: account.id.clone(),
                            state,
                            incarnation: account.incarnation,
                        })
                        .await;
                }
                Ok(snapshot)
            }
            Ok(snapshot) => Ok(snapshot),
            Err(error) => {
                tracing::debug!(account = %account.id, %error, "the quota poll got no snapshot");
                Err(error)
            }
        };
        (answer, profile_read)
    }

    /// The profile row's refresh. A failure is logged, a failed answer or a transport failure writing a `poll_failed`
    /// row beside its warning, and a reauth mark its acquisition landed reaches the router as `poll_account` reports
    /// one. A stored tier or class the profile changes asks the router to reload, since the snapshot's tier prices the
    /// router's own use of the account's windows and the class places the account among the paid sources. A profile
    /// that reports no plan is stored and flags the account plan ended, and one that reports a plan clears a flag that
    /// stands. A stored Team or Enterprise document whose tier `tables` holds no capacity row for logs one warning per
    /// account and tier.
    pub async fn refresh_profile(
        store: &Store,
        usage: &UsageService,
        tables: &PlanTables,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
    ) {
        Self::read_profile(
            store,
            usage,
            tables,
            ledger,
            router,
            account,
            PROFILE_SOURCE,
        )
        .await;
    }

    /// Whether the account's plan-ended flag stands, so the poll sends it no usage request.
    pub fn plan_paused(store: &Store, account_id: &str) -> bool {
        store
            .subscription_term(account_id)
            .ok()
            .flatten()
            .is_some_and(|term| term.plan_ended_at.is_some())
    }

    /// `refresh_profile` with the `source` a flag it sets or clears names, answering what the read found.
    async fn read_profile(
        store: &Store,
        usage: &UsageService,
        tables: &PlanTables,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
        source: &'static str,
    ) -> ProfileRead {
        let answer = usage.profile(&account.id).await;
        Self::report_landed_mark(usage, router, &account.id).await;
        // The profile row is the poll's to refresh: nothing else reads the endpoint after pairing, and the
        // admin views show what it says.
        match answer {
            Ok(profile) if reports_no_plan(&profile.raw) => {
                Self::flag_plan_ended(store, usage, ledger, router, account, &profile, source)
                    .await;
                ProfileRead::NoPlan
            }
            Ok(profile) => {
                let term = || Self::stored_term(store, &account.id);
                let before = term();
                if let Err(error) =
                    store.set_profile(&account.id, &profile.raw.to_string(), &profile.subscription)
                {
                    tracing::warn!(account = %account.id, %error, "the profile could not be stored");
                } else {
                    let after = term();
                    // The store's resolution decides the plan, so a usage-based Enterprise seat, which resolves
                    // `usage_based` first, never warns.
                    if let Some((Some(tier), _, Some(plan))) = &after {
                        warn_unpriced_tier(tables, usage.now(), &account.id, plan, tier);
                    }
                    if before != after {
                        if let Err(error) = router.reload().await {
                            tracing::debug!(
                                account = %account.id,
                                %error,
                                "the router did not reload for a new tier, class or plan"
                            );
                        }
                    }
                }
                Self::restore_plan(store, usage, ledger, router, account, &profile.raw).await;
                ProfileRead::Plan
            }
            Err(UsageError::Fault(fault)) => {
                Self::record_fault(usage, ledger, &account.id, &fault);
                ProfileRead::Unread
            }
            Err(error) => {
                tracing::debug!(account = %account.id, %error, "the quota poll got no profile");
                ProfileRead::Unread
            }
        }
    }

    /// Stores the document the read found and flags the account plan ended at the read's instant unless a flag stands,
    /// so the routing reads it as past its end. While the flag stands the account answers the found document's plan
    /// and tier, the readings taken before the end keeping the plan and the tier they were priced under, and the state
    /// stays as it is, since a renewed plan lifts the flag without a re-pair.
    async fn flag_plan_ended(
        store: &Store,
        usage: &UsageService,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
        profile: &ProfileResponse,
        source: &'static str,
    ) {
        usage.plan_ended(&account.id);
        let before = Self::stored_term(store, &account.id);
        // Stored before the flag, so the flag's reload brings the found plan and tier to the routing with it.
        if let Err(error) =
            store.set_profile(&account.id, &profile.raw.to_string(), &profile.subscription)
        {
            tracing::warn!(account = %account.id, %error, "the profile could not be stored");
        }
        let at = usage.now();
        match store.set_plan_ended(&account.id, at) {
            Ok(true) => {
                let organization = &profile.raw["organization"];
                let word = |key: &str| organization[key].as_str().unwrap_or("-").to_string();
                tracing::warn!(
                    account = %account.id,
                    organization_type = %word("organization_type"),
                    rate_limit_tier = %word("rate_limit_tier"),
                    subscription_status = %word("subscription_status"),
                    "the account's profile reports no plan, so the poll stops reading its usage"
                );
                ledger.record(plan_ended(&account.id, at, source, &profile.raw));
                if let Err(error) = router.reload().await {
                    tracing::debug!(account = %account.id, %error, "the router did not reload for an ended plan");
                }
            }
            // Neither a flag that stands nor the document's store bumps the revision, so a document that moved the
            // stored term reloads the router here, as a read that finds a plan does.
            Ok(false) => {
                if before != Self::stored_term(store, &account.id) {
                    if let Err(error) = router.reload().await {
                        tracing::debug!(
                            account = %account.id,
                            %error,
                            "the router did not reload for a new tier, class or plan"
                        );
                    }
                }
            }
            Err(error) => {
                tracing::warn!(account = %account.id, %error, "the ended plan could not be stored")
            }
        }
    }

    /// The stored term's tier, class and plan, which a profile read compares across its store to decide whether the
    /// router reloads. The class and the plan sit beside the tier, so a document whose seat tier or organization type
    /// alone moved reloads too.
    fn stored_term(
        store: &Store,
        account_id: &str,
    ) -> Option<(Option<String>, bool, Option<String>)> {
        store
            .subscription_term(account_id)
            .ok()
            .flatten()
            .map(|t| (t.tier, t.usage_based, t.plan))
    }

    /// Clears a plan-ended flag that stands, for a profile that reports a plan: the usage poll reads the account again
    /// from its next slot.
    async fn restore_plan(
        store: &Store,
        usage: &UsageService,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
        profile: &serde_json::Value,
    ) {
        match store.clear_plan_ended(&account.id) {
            Ok(true) => {
                usage.plan_restored(&account.id);
                tracing::info!(
                    account = %account.id,
                    "the account's profile reports a plan again, so the poll reads its usage"
                );
                ledger.record(plan_restored(
                    &account.id,
                    usage.now(),
                    PROFILE_SOURCE,
                    profile,
                ));
                if let Err(error) = router.reload().await {
                    tracing::debug!(account = %account.id, %error, "the router did not reload for a restored plan");
                }
            }
            Ok(false) => {}
            Err(error) => {
                tracing::warn!(account = %account.id, %error, "the restored plan could not be stored")
            }
        }
    }

    /// What a usage fetch this poll started found beyond its snapshot: a 401 sends the account to the re-pair, and a
    /// passing fault is recorded. The snapshot's own text and the schedule stay as the fetch left them. Answers whether
    /// the profile was read.
    async fn report_issue(
        store: &Store,
        usage: &UsageService,
        tables: &PlanTables,
        ledger: &Ledger,
        router: &RouterHandle,
        account: &AccountRow,
        issue: PollIssue,
    ) -> bool {
        match issue {
            PollIssue::Fault(fault) => {
                Self::record_fault(usage, ledger, &account.id, &fault);
                false
            }
            // The organization's refusal is what an ended plan answers, so the profile decides whether it is one.
            PollIssue::PlanRefused { fault, .. } => {
                let read = Self::read_profile(
                    store,
                    usage,
                    tables,
                    ledger,
                    router,
                    account,
                    USAGE_403_SOURCE,
                )
                .await;
                if read != ProfileRead::NoPlan {
                    Self::record_fault(usage, ledger, &account.id, &fault);
                }
                true
            }
            // The manager's fenced write moved the state before the report reaches the routing, so the transition row
            // the mirror writes names no detail, as a refresh failure's does.
            PollIssue::Unauthorized(refused) => {
                if let Some(mark) = usage.mark_auth_failed(&account.id, refused) {
                    Self::report_mark(router, mark).await;
                }
                false
            }
        }
    }

    /// One warning line and one `poll_failed` row for a poll that faulted, as often as polls fault: a fault arms no
    /// hold, so the poll interval spaces the rows.
    fn record_fault(usage: &UsageService, ledger: &Ledger, account_id: &str, fault: &PollFault) {
        let status = fault
            .status
            .map_or_else(|| "transport".to_string(), |status| status.to_string());
        tracing::warn!(
            account = %account_id,
            status = %status,
            error = %fault.text,
            "a poll of the {} endpoint failed",
            fault.endpoint
        );
        ledger.record(poll_failed(account_id, usage.now(), fault));
    }

    /// Reports the reauth mark the service's last acquisition for the account landed, as the relay reports its own:
    /// the manager's store write starts no reload, so without the report the routing would select the account until
    /// the next one.
    async fn report_landed_mark(usage: &UsageService, router: &RouterHandle, account_id: &str) {
        if let Some(mark) = usage.take_landed_mark(account_id) {
            Self::report_mark(router, mark).await;
        }
    }

    /// A mark the manager landed, a refresh's or a usage 401's, reported to the routing with no detail.
    async fn report_mark(router: &RouterHandle, mark: LandedMark) {
        router.report(Report::from(&mark)).await;
    }

    /// `poll_account`, then `refresh_profile` where the poll did not read the profile, for every
    /// account `polled` accepts, concurrently. An account counts the moment its snapshot was reported, and what has not
    /// reported by `bound` counts as pending and finishes in the background. An account flagged plan ended reads its
    /// profile alone and counts as paused.
    pub async fn sample_all(
        store: Arc<Store>,
        usage: Arc<UsageService>,
        ledger: Ledger,
        router: RouterHandle,
        tables: Arc<PlanTables>,
        bound: Duration,
    ) -> Sampled {
        let accounts: Vec<AccountRow> = store
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| polled(a.kind, a.state))
            .collect();
        let (paused, accounts): (Vec<AccountRow>, Vec<AccountRow>) = accounts
            .into_iter()
            .partition(|a| Self::plan_paused(&store, &a.id));
        // An account whose plan ended sends no usage request, so it reports no sample and counts as paused, and its
        // profile read is the start's as every account's is.
        let paused_count = paused.len();
        for account in paused {
            let (store, usage, tables, task_ledger, router) = (
                store.clone(),
                usage.clone(),
                tables.clone(),
                ledger.clone(),
                router.clone(),
            );
            ledger.track(tokio::spawn(async move {
                Self::refresh_profile(&store, &usage, &tables, &task_ledger, &router, &account)
                    .await;
            }));
        }
        let total = accounts.len();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
        for account in accounts {
            let (store, usage, tables, task_ledger, router, tx) = (
                store.clone(),
                usage.clone(),
                tables.clone(),
                ledger.clone(),
                router.clone(),
                tx.clone(),
            );
            // Tracked so a fetch still stalled when `ledger.shutdown` runs is aborted with it, rather than
            // outliving the process holding its `Arc<Store>`.
            let handle = tokio::spawn(async move {
                let (answer, profile_read) =
                    Self::poll_reading(&store, &usage, &tables, &task_ledger, &router, &account)
                        .await;
                let _ = tx.send(matches!(answer, Ok(ref s) if s.error.is_none()));
                if !profile_read {
                    Self::refresh_profile(&store, &usage, &tables, &task_ledger, &router, &account)
                        .await;
                }
            });
            ledger.track(handle);
        }
        drop(tx);
        let mut sampled = Sampled {
            paused: paused_count,
            ..Sampled::default()
        };
        let _ = tokio::time::timeout(bound, async {
            while sampled.reported + sampled.failed < total {
                match rx.recv().await {
                    Some(true) => sampled.reported += 1,
                    Some(false) => sampled.failed += 1,
                    None => break,
                }
            }
        })
        .await;
        sampled.pending = total - sampled.reported - sampled.failed;
        sampled
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use phoebis_oauth::{ExtraUsage, UsageSnapshot, UsageWindow};

    #[test]
    fn the_next_slot_keeps_the_phase_and_skips_the_slots_a_late_wake_missed() {
        let start = Instant::now();
        let at = |ms: u64| start + Duration::from_millis(ms);
        let slot = at(300_000);
        assert_eq!(
            next_slot(slot, at(300_001)),
            at(600_000),
            "a read just past its slot keeps the phase"
        );
        assert_eq!(
            next_slot(slot, at(1_200_000)),
            at(1_500_000),
            "a read three slots late skips them"
        );
        assert_eq!(
            next_slot(slot, at(1_499_000)),
            at(1_800_000),
            "and the next slot comes no sooner than an interval after the read, less the tolerance"
        );
    }

    #[test]
    fn a_head_yields_one_row_per_window_present_and_the_overage_row() {
        let headers: Vec<(String, String)> = [
            ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
            ("anthropic-ratelimit-unified-5h-reset", "2000"),
            ("anthropic-ratelimit-unified-7d-utilization", "0.13"),
            ("anthropic-ratelimit-unified-7d_oi-utilization", "0.91"),
            ("anthropic-ratelimit-unified-overage-in-use", "true"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.5"),
            ("anthropic-ratelimit-unified-overage-reset", "3000"),
        ]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
        let unified = UnifiedHeaders::parse_list(&headers).unwrap();
        let (quota, extra) = samples_from_unified("a", &unified, 1_234_567, false);
        assert_eq!(
            quota
                .iter()
                .map(|q| (
                    q.window.as_str(),
                    q.utilization,
                    q.reset_at,
                    q.bucket,
                    q.source.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("five_hour", Some(0.42), Some(2_000_000), 1_200, "head"),
                ("seven_day", Some(0.13), None, 1_200, "head"),
                (
                    "seven_day_overage_included",
                    Some(0.91),
                    None,
                    1_200,
                    "head"
                )
            ]
        );
        let extra = extra.unwrap();
        assert_eq!(
            (
                extra.is_enabled,
                extra.in_use,
                extra.utilization,
                extra.overage_reset,
                extra.used_credits,
                extra.source.as_str()
            ),
            (true, Some(true), Some(0.5), Some(3_000_000), None, "head")
        );
    }

    #[test]
    fn a_poll_yields_every_window_under_its_key_and_the_fable_window_and_credits() {
        let window = |key: &str, u: f64| UsageWindow {
            key: key.into(),
            utilization: Some(u),
            resets_at: Some(9_000_000),
        };
        let snapshot = UsageSnapshot {
            account_id: "a".into(),
            identity_generation: 1,
            fetched_at: 600_500,
            windows: vec![window("five_hour", 12.5), window("seven_day_opus", 3.0)],
            fable_window: Some(window("fable", 42.0)),
            extra_usage: Some(ExtraUsage {
                is_enabled: true,
                monthly_limit: Some(5_000),
                used_credits: Some(1_234),
                utilization: Some(0.2468),
                currency: Some("USD".into()),
                decimal_places: None,
                disabled_reason: None,
            }),
            raw_keys: vec![],
            error: None,
            spend: None,
        };
        let (quota, extra) = samples_from_poll(&snapshot);
        assert_eq!(
            quota
                .iter()
                .map(|q| (
                    q.window.as_str(),
                    q.utilization,
                    q.bucket,
                    q.source.as_str()
                ))
                .collect::<Vec<_>>(),
            [
                ("five_hour", Some(0.125), 600, "poll"),
                ("seven_day_opus", Some(0.03), 600, "poll"),
                ("seven_day_fable", Some(0.42), 600, "poll")
            ]
        );
        let extra = extra.unwrap();
        assert_eq!(
            (
                extra.monthly_limit,
                extra.used_credits,
                extra.utilization,
                extra.currency.as_deref(),
                extra.in_use
            ),
            (
                Some(5_000),
                Some(1_234),
                Some(0.2468 / 100.0),
                Some("USD"),
                None
            )
        );
    }
}
