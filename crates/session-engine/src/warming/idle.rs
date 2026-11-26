//! Main's idle windows and the idle-age counters they raise per pool and UTC day.

use phoebis_routing::{RegistrySnapshot, DAY_MS};

use crate::sessions::state::{
    agent_ref, Agent, AgentState, SessionKey, SessionsState, AGENT_HELD, MAIN_AGENT,
};
use crate::sessions::view::CompanionState;
use crate::warming::agent::{warming_mut, AgentWarming};
use crate::warming::causes::Reason;
use crate::warming::state::{Changes, WarmingState};

/// The UTC day `ms` falls on, counted from the epoch. The day counters and their reader both go through it, so the
/// two never disagree on where a day ends.
pub(crate) fn utc_day(ms: i64) -> i64 {
    ms.div_euclid(DAY_MS)
}

/// The lower bounds of the ten idle-age buckets, in seconds: 0 to 5, 5 to 15, 15 to 30 and 30 to 60 minutes, then
/// 1 to 2, 2 to 4, 4 to 8, 8 to 16 and 16 to 32 hours, and above.
pub const IDLE_AGE_BOUNDS_SECONDS: [u64; 10] = [
    0, 300, 900, 1_800, 3_600, 7_200, 14_400, 28_800, 57_600, 115_200,
];

/// The bucket of an idle window `age_ms` old. An age below zero, from a clock that stepped back, reads as the first.
pub fn idle_age_bucket(age_ms: i64) -> usize {
    IDLE_AGE_BOUNDS_SECONDS
        .iter()
        .rposition(|&bound| age_ms >= bound as i64 * 1_000)
        .unwrap_or(0)
}

/// One idle-age bucket of a pool: the idle windows of main that reached the bucket's start, and those whose next
/// request from main continued the same prefix inside the bucket.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdleReturnBucket {
    pub reached: u64,
    pub returned: u64,
}

/// One pool's counters, a bucket per bound of `IDLE_AGE_BOUNDS_SECONDS`.
#[derive(Clone, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct IdleReturns {
    pub buckets: [IdleReturnBucket; 10],
}

/// One open idle window of main: when it began, the highest bucket `reached` has been raised for, the prefix main's
/// last admitted request carried when the window opened (kept apart from the recipe, so a return is judged against it
/// after a ceiling discarded the recipe and on a pool that retains none), the moment a prompt announced the return,
/// when one has, and the pool the window counts under, fixed at the open so a key that moves pools mid-window keeps
/// the window's buckets in one pool.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct IdleEpisode {
    pub started: i64,
    pub bucket: Option<usize>,
    pub anchor: Option<PrefixAnchor>,
    pub prompt_at: Option<i64>,
    pub pool_key: String,
    /// The UTC day the episode's highest counted bucket was reached, where its return counts, so a day never
    /// answers a return without its reach. `None` until a bucket is counted, and on a window restored from a row saved
    /// without it, whose reach is held under no day, so its return counts nothing unless the request raises a bucket.
    pub bucket_day: Option<i64>,
}

/// The prefix an idle window judges a return against: the request's static hash, its last message boundary, the one
/// before it (`None` for a one-message chain) and the deepest marked one before those two. A return holds any of the
/// three.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PrefixAnchor {
    pub static_hash: String,
    pub last: String,
    pub before_last: Option<String>,
    /// The deepest boundary before the last two that the request marked with `cache_control`, which the upstream
    /// reads the cache through.
    pub marked: Option<String>,
}

/// Whether a request continues the prefix `anchor` names: one of its message boundaries equals the anchor's last
/// boundary, the one before it or its marked one, so a return counts when the client re-sent the last message in a
/// shape the canonical form does not restore and the message before it held, and when an interrupted turn's next
/// request dropped both and kept the marked boundary the upstream reads the cache through. The chain is the only
/// evidence, and the request's class is none: a refreshing class whose chain holds none of them answers false where
/// `guard` re-arms it, as do a request with no chain and an absent anchor.
pub fn continues_prefix(anchor: Option<&PrefixAnchor>, boundaries: &[String]) -> bool {
    anchor.is_some_and(|anchor| {
        boundaries.iter().any(|boundary| {
            *boundary == anchor.last
                || anchor.before_last.as_ref() == Some(boundary)
                || anchor.marked.as_ref() == Some(boundary)
        })
    })
}

/// Raises `reached` in the buckets after `counted` up to `bucket`. A tick and the request that ends a window both
/// count through here, and `counted` is the fence that keeps them from raising one bucket twice for one window. A
/// bucket at or below the fence raises nothing: the wall clock can step back, and a tick can take the lock with a
/// later `now` than a request or a report that read the clock before it.
fn raise_reached(counters: &mut IdleReturns, counted: Option<usize>, bucket: usize) {
    let from = counted.map_or(0, |c| c + 1);
    for counter in counters.buckets.iter_mut().take(bucket + 1).skip(from) {
        counter.reached += 1;
    }
}

/// The pool a window counts under, read once at its open: the pool of the session's key in the snapshot, else the
/// agent's own pool id, else the pinned account's own id for an account outside every pool, so no window counts under
/// the empty key, which no pool row reads. The key comes first because the agent's pool id is a copy only an admitted
/// selection or a retained recipe's tick refreshes.
fn idle_returns_key(
    snapshot: &RegistrySnapshot,
    agent: &Agent,
    warming: &AgentWarming,
) -> Option<String> {
    snapshot
        .key_pool(&agent.key_id)
        .map(|pool| pool.id.clone())
        .or_else(|| (!warming.pool_id.is_empty()).then(|| warming.pool_id.clone()))
        .or_else(|| warming.account.clone())
}

/// Whether main's state holds an idle window: the opener and the tick's counter read the same states.
fn holds_idle_window(state: AgentState) -> bool {
    !matches!(
        state,
        AgentState::Busy | AgentState::Released | AgentState::Unknown
    )
}

/// Opens main's idle window when none is counted and main is idle or parked, from a start past the floor. The report
/// that moves main there opens it, so a prompt inside the same second still ends a window, and the tick opens the
/// ones a presence, a park's end or a load left unopened. True when a window opened.
pub(crate) fn open_idle_episode(
    main: &Agent,
    warming: &mut AgentWarming,
    snapshot: &RegistrySnapshot,
) -> bool {
    if warming.idle_episode.is_some() || !holds_idle_window(main.state) {
        return false;
    }
    let start = main.idle_window_start();
    if warming
        .idle_window_floor
        .is_some_and(|floor| start <= floor)
    {
        return false;
    }
    // One lookup per window, so a key that moves pools takes its next window with it.
    let Some(pool_key) = idle_returns_key(snapshot, main, warming) else {
        return false;
    };
    warming.idle_episode = Some(IdleEpisode {
        started: start,
        bucket: None,
        anchor: warming.last_prefix.clone(),
        prompt_at: None,
        pool_key,
        bucket_day: None,
    });
    true
}

impl WarmingState {
    /// The first UTC day the retention keeps when `day` is the newest, `None` for a retention of 0, which keeps every
    /// day. The roll drops the days before it and a return counts only from it on, so the two agree on the cutoff.
    fn first_kept_day(&self, day: i64) -> Option<i64> {
        let retention = self.timing.idle_return_retention_days;
        (retention != 0).then(|| day - (i64::from(retention) - 1))
    }

    /// Drops the days the retention no longer keeps once `now` falls past the UTC day the counters last counted under,
    /// and records the cutoff for the next save, which takes the registry's rows of those days. The two sites that
    /// count call it, as `AgentWarming::roll_counters_day` is called, and the tick is one of them, so a day leaves
    /// within a tick of the midnight that ages it out.
    fn roll_idle_returns_day(&mut self, now: i64) {
        let day = utc_day(now);
        if day <= self.idle_returns_day {
            return;
        }
        self.idle_returns_day = day;
        let Some(cutoff) = self.first_kept_day(day) else {
            return;
        };
        let mut dropped = false;
        for days in self.idle_returns.values_mut() {
            let kept = days.split_off(&cutoff);
            dropped |= !days.is_empty();
            *days = kept;
        }
        self.idle_returns.retain(|_, days| !days.is_empty());
        if dropped {
            self.purge_counters_before = Some(cutoff);
        }
    }

    /// Advances main's idle window on every attended session: opens one where none is counted and raises `reached`
    /// up to the window's current bucket. It reads the sessions, never the retained recipes, so a session whose
    /// recipe a ceiling discarded, or that never sent a request, still reaches the later buckets. A busy main keeps
    /// its window and counts nothing, and a window whose return a prompt announced waits for the request.
    pub(crate) fn advance_idle_episodes(
        &mut self,
        sessions: &SessionsState,
        snapshot: &RegistrySnapshot,
        now: i64,
    ) -> Changes {
        let mut changes = Changes::default();
        self.roll_idle_returns_day(now);
        let day = utc_day(now);
        for (key, session) in sessions.entries.iter() {
            if session.companion() != CompanionState::Reporting {
                continue;
            }
            let Some(main) = session.agents.get(MAIN_AGENT) else {
                continue;
            };
            if !holds_idle_window(main.state) {
                continue;
            }
            let warming = warming_mut(&mut self.agents, key, MAIN_AGENT).expect(AGENT_HELD);
            if open_idle_episode(main, warming, snapshot) {
                changes.agent_row(key, MAIN_AGENT);
            }
            let Some(episode) = warming.idle_episode.as_mut() else {
                continue;
            };
            if episode.prompt_at.is_some() {
                continue;
            }
            let bucket = idle_age_bucket(now - episode.started);
            // Only a window that crossed into a new bucket is counted and written, so an idle session costs no save
            // per tick and a day holds no empty counters.
            if episode.bucket.is_some_and(|counted| bucket <= counted) {
                continue;
            }
            changes.agent_row(key, MAIN_AGENT);
            changes.pool_row(&episode.pool_key);
            raise_reached(
                self.idle_returns
                    .entry(episode.pool_key.clone())
                    .or_default()
                    .entry(day)
                    .or_default(),
                episode.bucket,
                bucket,
            );
            episode.bucket = Some(bucket);
            episode.bucket_day = Some(day);
        }
        changes
    }

    /// Main's request ends its open idle window: the buckets the window crossed since the last tick count as reached,
    /// and the bucket of its age counts a return when the request continues the prefix the window opened on. The age
    /// runs to the prompt that announced the request, when one did, since the user came back then however many ticks
    /// pass before the request arrives.
    pub(crate) fn end_idle_episode(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        boundaries: &[String],
        now: i64,
    ) -> Changes {
        let mut changes = Changes::default();
        self.roll_idle_returns_day(now);
        let Some(main) = agent_ref(&sessions.entries, key, MAIN_AGENT) else {
            return changes;
        };
        let warming = warming_mut(&mut self.agents, key, MAIN_AGENT).expect(AGENT_HELD);
        let Some(episode) = warming.close_idle_episode(main) else {
            return changes;
        };
        // Never below the fence: the window already counted as reaching it, so its return is read there.
        let bucket = idle_age_bucket(episode.prompt_at.unwrap_or(now) - episode.started)
            .max(episode.bucket.unwrap_or(0));
        let today = utc_day(now);
        let raised = episode.bucket.is_none_or(|counted| bucket > counted);
        // The return's bucket is the highest the window counted, so the return counts on the day that bucket was
        // reached (today where this request raised it), and no day answers a return without its reach. Where that
        // day has left at the roll, or a window restored from a row saved without it holds its reach under no day,
        // the return leaves with its reach and counts nothing.
        let return_day = if raised {
            Some(today)
        } else {
            episode.bucket_day.filter(|day| {
                self.first_kept_day(self.idle_returns_day)
                    .is_none_or(|first| *day >= first)
            })
        }
        .filter(|_| continues_prefix(episode.anchor.as_ref(), boundaries));
        changes.agent_row(key, MAIN_AGENT);
        if raised || return_day.is_some() {
            let days = self
                .idle_returns
                .entry(episode.pool_key.clone())
                .or_default();
            if raised {
                raise_reached(days.entry(today).or_default(), episode.bucket, bucket);
            }
            if let Some(day) = return_day {
                days.entry(day).or_default().buckets[bucket].returned += 1;
            }
            changes.pool_row(&episode.pool_key);
        }
        changes
    }

    /// Closes the agent's idle window with no return.
    pub(crate) fn close_idle_window(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        agent_id: &str,
    ) {
        let agent = agent_ref(&sessions.entries, key, agent_id).expect(AGENT_HELD);
        warming_mut(&mut self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .close_idle_episode(agent);
    }

    /// Clears the floor the next idle window's start has to pass, for an event that moves the start on purpose.
    pub(crate) fn clear_idle_floor(&mut self, key: &SessionKey, agent_id: &str) {
        warming_mut(&mut self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .idle_window_floor = None;
    }

    /// Stamps the instant a prompt announced the return on the open idle window, or clears it where `at` is `None`.
    pub(crate) fn prompt(&mut self, key: &SessionKey, agent_id: &str, at: Option<i64>) {
        if let Some(episode) = warming_mut(&mut self.agents, key, agent_id)
            .expect(AGENT_HELD)
            .idle_episode
            .as_mut()
        {
            episode.prompt_at = at;
        }
    }

    /// Opens main's idle window behind a report that moved main to idle or parked.
    pub(crate) fn open_idle_window(
        &mut self,
        sessions: &SessionsState,
        key: &SessionKey,
        snapshot: &RegistrySnapshot,
    ) {
        if let Some(main) = agent_ref(&sessions.entries, key, MAIN_AGENT) {
            let warming = warming_mut(&mut self.agents, key, MAIN_AGENT).expect(AGENT_HELD);
            open_idle_episode(main, warming, snapshot);
        }
    }

    /// A lease that lapsed: the lapse is counted once for each agent of the session, and main's window closes.
    pub(crate) fn lease_lapsed(&mut self, sessions: &SessionsState, key: &SessionKey) {
        // The recipes stay: a silent companion is not an ended session, and the idle limits bound what the
        // engine spends on it. The lapse is counted once per agent, as the Go engine counted it, but no
        // agent's own reason is rewritten: a recipe the scheduler keeps probing would otherwise read held
        // in every view until its next probe.
        let session = &sessions.entries[key];
        let agents = session.agents.len() as u64;
        *self.reasons.entry(Reason::LeaseExpired).or_default() += agents;
        // A lapse is no return, so main's window ends here.
        if let Some(main) = session.agents.get(MAIN_AGENT) {
            warming_mut(&mut self.agents, key, MAIN_AGENT)
                .expect(AGENT_HELD)
                .close_idle_episode(main);
        }
    }
}
