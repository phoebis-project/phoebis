//! Quota samples from two sources: every response head of an OAuth account, and a poll of the usage endpoint
//! every five minutes per account. Heads cost nothing and cover active accounts; the poll fills idle gaps and is
//! the only source of extra-usage credit totals.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use phoebis_oauth::{UsageService, UsageSnapshot};
use phoebis_relay::UnifiedHeaders;
use phoebis_routing::{credits_from_usage, sample_from_usage, Report, RouterHandle};
use phoebis_store::{AccountKind, AccountRow, AccountState, Store};
use phoebis_transport::response::ResponseHead;
use tokio::task::JoinSet;
use tokio::time::Instant;

use crate::assemble::Record;
use crate::record::{ExtraUsageSampleRecord, QuotaSampleRecord};
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

/// The profile is read again on every twelfth poll of an account, an hour apart: a tier moves rarely, and the
/// profile endpoint shares the usage endpoint's per-account limit.
pub const PROFILE_EVERY_POLLS: u32 = 12;

pub fn samples_from_head(
    account_id: &str,
    headers: &[(String, String)],
    observed_at: i64,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let head = ResponseHead {
        status: 200,
        headers: headers.to_vec(),
    };
    match UnifiedHeaders::parse(&head) {
        Some(unified) => samples_from_unified(account_id, &unified, &head, observed_at),
        None => (Vec::new(), None),
    }
}

/// The assembler's entry: it parsed the head once for the attempt row and hands that parse here rather than
/// paying for a second clone and parse on every response head.
pub(crate) fn samples_from_unified(
    account_id: &str,
    unified: &UnifiedHeaders,
    head: &ResponseHead,
    observed_at: i64,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let bucket = bucket_of(observed_at);
    let float = |name: &str| {
        head.get(name)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .filter(|f| f.is_finite())
    };
    let mut quota = Vec::new();
    let windows = [
        ("5h", unified.five_hour_utilization, unified.five_hour_reset),
        ("7d", unified.seven_day_utilization, unified.seven_day_reset),
        (
            "7d_oi",
            float("anthropic-ratelimit-unified-7d_oi-utilization"),
            unified.seven_day_oi_reset,
        ),
    ];
    for (window, utilization, reset_at) in windows {
        if utilization.is_some() || reset_at.is_some() {
            quota.push(QuotaSampleRecord {
                account_id: account_id.into(),
                bucket,
                window: window.into(),
                utilization,
                reset_at,
                source: "header".into(),
                observed_at: Some(observed_at),
            });
        }
    }
    let extra = unified.overage_in_use.map(|in_use| ExtraUsageSampleRecord {
        account_id: account_id.into(),
        bucket,
        is_enabled: true,
        in_use: Some(in_use),
        monthly_limit: None,
        used_credits: None,
        utilization: float("anthropic-ratelimit-unified-overage-utilization"),
        currency: None,
        disabled_reason: unified.overage_disabled_reason.clone(),
        overage_reset: unified.overage_reset,
        source: "header".into(),
    });
    (quota, extra)
}

pub fn samples_from_poll(
    snapshot: &UsageSnapshot,
) -> (Vec<QuotaSampleRecord>, Option<ExtraUsageSampleRecord>) {
    let bucket = bucket_of(snapshot.fetched_at);
    // The endpoint reports percent; the headers report a fraction. One scale in the table.
    let row = |key: &str, utilization: Option<f64>, reset_at: Option<i64>| QuotaSampleRecord {
        account_id: snapshot.account_id.clone(),
        bucket,
        window: key.into(),
        utilization: utilization.map(|u| u / 100.0),
        reset_at,
        source: "poll".into(),
        observed_at: Some(snapshot.fetched_at),
    };
    let mut quota: Vec<QuotaSampleRecord> = snapshot
        .windows
        .iter()
        .map(|w| row(&w.key, w.utilization, w.resets_at))
        .collect();
    if let Some(fable) = &snapshot.fable_window {
        quota.push(row("fable", fable.utilization, fable.resets_at));
    }
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
            utilization: e.utilization,
            currency: e.currency.clone(),
            disabled_reason: e.disabled_reason.clone(),
            overage_reset: None,
            source: "poll".into(),
        });
    (quota, extra)
}

/// Whether the poll asks about this account: every OAuth account that holds a credential, paused or not,
/// because a paused account's quota is still the operator's to watch.
pub fn polled(kind: AccountKind, state: AccountState) -> bool {
    kind == AccountKind::Oauth
        && !matches!(state, AccountState::Disconnected | AccountState::Unpaired)
}

/// What the startup step found before the bound: `reported` accounts reached the router with a sample,
/// `failed` answered no clean snapshot, `pending` had reported nothing when the bound passed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Sampled {
    pub reported: usize,
    pub failed: usize,
    pub pending: usize,
}

/// One polled account's place in the schedule.
struct Slot {
    /// The row the attempt is made with, kept so a registry read that fails leaves the schedule as it was.
    account: AccountRow,
    /// The account's incarnation the slot was drawn for: a re-pairing is a new account to the schedule, and a
    /// completion drawn for another incarnation moves nothing here.
    incarnation: u64,
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
        .map(|(id, slot)| (id.clone(), slot.incarnation))
        .collect();
    if drawn == known {
        return;
    }
    slots.retain(|id, slot| known.contains(&(id.clone(), slot.incarnation)));
    for (index, account) in accounts.iter().enumerate() {
        let slot = slots.entry(account.id.clone()).or_insert_with(|| Slot {
            account: account.clone(),
            incarnation: account.incarnation,
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
    /// and reported to the router as `poll_account` does with a router.
    ///
    /// Spawned once the startup step is done: that step has just read every account, so an account the first
    /// pass finds is next read an interval after the start, on its phase, and its profile an hour on.
    pub fn spawn(
        store: Arc<Store>,
        usage: Arc<UsageService>,
        ledger: Ledger,
        router: RouterHandle,
    ) -> tokio::task::JoinHandle<()> {
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
                    let (account, incarnation) = (slot.account.clone(), slot.incarnation);
                    let (store, usage, ledger, router) =
                        (store.clone(), usage.clone(), ledger.clone(), router.clone());
                    let handle = attempts.spawn(async move {
                        Self::poll_account(&usage, &ledger, Some(&router), &account).await;
                        if profile_due {
                            Self::refresh_profile(&store, &usage, &router, &account).await;
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
                                if slot.in_flight == Some(id) && slot.incarnation == done.incarnation {
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

    /// One account's snapshot: recorded to the ledger when it is clean and, when `router` is given, reported
    /// to it as `Report::Sample` and, when the snapshot carries the evidence, `Report::Overage` and
    /// `Report::Credits`, before this returns; the clean snapshot is answered. The scheduler and the startup step
    /// both call it with the router. The profile is `refresh_profile`'s.
    pub async fn poll_account(
        usage: &UsageService,
        ledger: &Ledger,
        router: Option<&RouterHandle>,
        account: &AccountRow,
    ) -> Option<UsageSnapshot> {
        // The service's hold, concurrency cap and coalescing apply and its cache TTL does not; a snapshot with
        // an error is a stale one, and a stale sample must not be written under a fresh bucket.
        match usage.poll(&account.id).await {
            Ok(snapshot) if snapshot.error.is_none() => {
                let (quota, extra) = samples_from_poll(&snapshot);
                ledger.record(Record::Samples { quota, extra });
                if let Some(router) = router {
                    // Reported before the profile refresh, so a stalled profile endpoint holds no sample back
                    // from selection; both halves go, as the selection's own pull sends them, so the first
                    // selection sees the overage state too.
                    let (sample, overage) = sample_from_usage(&snapshot, account.incarnation);
                    router.report(Report::Sample {
                        account_id: account.id.clone(),
                        sample,
                        incarnation: account.incarnation,
                    });
                    if let Some(evidence) = overage {
                        router.report(Report::Overage {
                            account_id: account.id.clone(),
                            evidence,
                            incarnation: account.incarnation,
                        });
                    }
                    if let Some(state) = credits_from_usage(&snapshot, account.incarnation) {
                        router.report(Report::Credits {
                            account_id: account.id.clone(),
                            state,
                            incarnation: account.incarnation,
                        });
                    }
                }
                Some(snapshot)
            }
            Ok(_) => None,
            Err(error) => {
                tracing::debug!(account = %account.id, %error, "the quota poll got no snapshot");
                None
            }
        }
    }

    /// The profile row's refresh; a failure is logged and nothing else. A stored tier the profile changes asks
    /// the router to reload, since the snapshot's tier prices the router's own use of the account's windows.
    pub async fn refresh_profile(
        store: &Store,
        usage: &UsageService,
        router: &RouterHandle,
        account: &AccountRow,
    ) {
        // The profile row is the poll's to refresh: nothing else reads the endpoint after pairing, and the
        // admin views show what it says.
        match usage.profile(&account.id).await {
            Ok(profile) => {
                let before = store
                    .subscription(&account.id)
                    .ok()
                    .flatten()
                    .and_then(|s| s.tier);
                if let Err(error) =
                    store.set_profile(&account.id, &profile.raw.to_string(), &profile.subscription)
                {
                    tracing::warn!(account = %account.id, %error, "the profile could not be stored");
                } else if before != profile.subscription.tier {
                    if let Err(error) = router.reload().await {
                        tracing::debug!(account = %account.id, %error, "the router did not reload for a new tier");
                    }
                }
            }
            Err(error) => {
                tracing::debug!(account = %account.id, %error, "the quota poll got no profile")
            }
        }
    }

    /// `poll_account` with the router, then `refresh_profile`, for every account `polled` accepts,
    /// concurrently; an account counts the moment its snapshot was reported, and what has not reported by
    /// `bound` counts as pending and finishes in the background.
    pub async fn sample_all(
        store: Arc<Store>,
        usage: Arc<UsageService>,
        ledger: Ledger,
        router: RouterHandle,
        bound: Duration,
    ) -> Sampled {
        let accounts: Vec<AccountRow> = store
            .accounts()
            .unwrap_or_default()
            .into_iter()
            .filter(|a| polled(a.kind, a.state))
            .collect();
        let total = accounts.len();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<bool>();
        for account in accounts {
            let (store, usage, task_ledger, router, tx) = (
                store.clone(),
                usage.clone(),
                ledger.clone(),
                router.clone(),
                tx.clone(),
            );
            // Tracked so a fetch still stalled when `ledger.shutdown` runs is aborted with it, rather than
            // outliving the process holding its `Arc<Store>`.
            let handle = tokio::spawn(async move {
                let answer =
                    Self::poll_account(&usage, &task_ledger, Some(&router), &account).await;
                let _ = tx.send(answer.is_some());
                Self::refresh_profile(&store, &usage, &router, &account).await;
            });
            ledger.track(handle);
        }
        drop(tx);
        let mut sampled = Sampled::default();
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
            ("anthropic-ratelimit-unified-overage-in-use", "true"),
            ("anthropic-ratelimit-unified-overage-utilization", "0.5"),
            ("anthropic-ratelimit-unified-overage-reset", "3000"),
        ]
        .iter()
        .map(|(n, v)| (n.to_string(), v.to_string()))
        .collect();
        let (quota, extra) = samples_from_head("a", &headers, 1_234_567);
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
                ("5h", Some(0.42), Some(2_000_000), 1_200, "header"),
                ("7d", Some(0.13), None, 1_200, "header")
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
            (true, Some(true), Some(0.5), Some(3_000_000), None, "header")
        );
        assert_eq!(samples_from_head("a", &[], 1), (vec![], None));
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
                disabled_reason: None,
            }),
            raw_keys: vec![],
            error: None,
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
                ("fable", Some(0.42), 600, "poll")
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
            (Some(5_000), Some(1_234), Some(0.2468), Some("USD"), None)
        );
    }
}
