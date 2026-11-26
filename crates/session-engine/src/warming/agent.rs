//! The warming's record of an agent, held in a map of its own beside the session engine's record of the same agent.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::sessions::state::{Agent, SessionKey};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::KeepaliveTiming;
use crate::warming::guard::Guard;
use crate::warming::idle::{utc_day, IdleEpisode, PrefixAnchor};
use crate::warming::observation::{
    AsideRecipe, Kept, Observation, OpenCompaction, Settlement, UnansweredProbe,
};
use crate::warming::usage::ProbeUsage;

/// The warming's record of one agent: its recipe, its schedule, its probes and its idle window. One stands for each
/// agent record of the session engine, made and dropped in the same hold as that record.
pub(crate) struct AgentWarming {
    /// The generation of the key the recipe was retained under, off the observed request. A probe's and a
    /// settlement's rows file under it.
    pub key_generation: i64,
    /// The open idle window of main, `None` while none is counted.
    pub idle_episode: Option<IdleEpisode>,
    /// The start of main's last closed idle window. The next opens only from a start past it, so a close that leaves
    /// the start in place, a foreign request or a lapse a registration adopts again, never reopens the same window.
    /// An event that moves the next window's start on purpose (an `agent_start` of a released main, a park's end, a
    /// presence report, a request, a revival) clears it: the floor orders starts, and a clock that stepped back before
    /// that event is no ordering.
    pub idle_window_floor: Option<i64>,
    /// The anchor of main's last admitted request, retained or not, which the next idle window judges a return against
    /// and the chain watch reads the next request's chain against.
    pub last_prefix: Option<PrefixAnchor>,
    /// The previous main request's prompt as its completion reported it: uncached input, cache reads and both
    /// cache writes, set by every pending main completion and every marked one, so a refused recipe's unmarked
    /// request leaves it as it was. What the watch reads the next completion's cache reads against.
    pub last_prompt_tokens: Option<u64>,
    pub current: Option<Observation>,
    /// Never saved: a restart keeps the current observation alone.
    pub aside: Option<AsideRecipe>,
    /// The compaction open on the agent, held apart from `current` so none of its events moves the recipe. Never saved:
    /// a restart drops it with the events that would have completed it.
    pub compaction: Option<OpenCompaction>,
    /// The recipe was set or replaced since the last landed save that carried its body, so the next save seals it.
    pub body_dirty: bool,
    /// Whether the registry may hold a sealed body under this agent's row, so a save without a recipe clears it. A new
    /// record starts true, since it can take over a row an earlier record sealed a body into, and an acknowledged seal
    /// or clear sets it to what the registry then holds.
    pub body_saved: bool,
    /// When the entry the recipe keeps warm expires. The due time is this less the timing's lead and jitter.
    pub expires: Option<i64>,
    /// The share of the timing's jitter this schedule takes, rolled once per schedule so the due time holds
    /// still between ticks.
    pub jitter_frac: f64,
    /// An outstanding backoff after a failed probe. The due time is the later of it and the expiry's.
    pub retry_at: Option<i64>,
    /// The engine's timing, held here so a due time and a retry never need the engine.
    pub timing: KeepaliveTiming,
    pub cache_started: i64,
    pub ttl_ms: i64,
    pub cold: u32,
    pub failures: u32,
    pub account: Option<String>,
    /// The incarnation of `account` when the armed recipe was last answered on it. `None` on a record saved before
    /// agents carried one, which holds under `pin_moved` until the agent's next request arms the recipe.
    pub account_incarnation: Option<u64>,
    pub pool_id: String,
    /// The canonical model of the request the recipe came from, as the relay resolved it: the price lookups,
    /// the rollup keys, the cooldown scope, and the views read this, never the caller's spelling in the recipe.
    pub canonical_model: String,
    /// The entrypoint of the observed request's `client` label, what its probes price under, and `None` for a label
    /// that names none.
    pub entrypoint: Option<String>,
    pub reason: Reason,
    /// The guard's verdict on the last request the agent observed, `None` before the first one. Never `reason`
    /// itself: an auxiliary or a foreign request never reaches `reason`, which stays the recipe's own gating
    /// state, so a hold's `began` check, the completion's gate-reason match, and the view's armed-or-held state
    /// all read it undisturbed.
    pub last_guard: Option<Guard>,
    /// Whether the current hold has already ledgered the probe it withheld. A hold can begin before its
    /// probe is due, so the skip is counted when the due time passes rather than where the hold started.
    pub hold_counted: bool,
    pub paused: bool,
    pub in_flight: bool,
    pub probes: u64,
    pub usage: ProbeUsage,
    /// Every probe since the last real request with the entry it wrote, settled as wasted when the recipe stops, and
    /// the newest covered one, else the newest, the coverage the next request reads.
    pub unanswered: Vec<UnansweredProbe>,
    /// The probes a superseding real request took over from `unanswered`, with the keys they were paid under: the
    /// request moves the agent's model, account, pool and key generation before its completion decides them. A
    /// probe sent while it is open lands on `unanswered`, for the request after it. The idle limit moves its probes
    /// here too, pending under its label until their entry lapses or a turn reads them, and a failed turn hands back
    /// the probes it took the same way.
    pub kept: Vec<Kept>,
    pub last_real_at: Option<i64>,
    /// The arrival of the last request whose completion succeeded with a 2xx status, or of a turn whose completion the
    /// engine missed, which the next arrival or a bus gap ends with none observed. The kept-warm gap is measured from
    /// it rather than from `last_real_at`, so a failed turn, which stays no evidence about the cache, leaves the retry
    /// after it the gap from the last turn that answered.
    pub last_answered_at: Option<i64>,
    /// The label of the last discard that had one, so a probe result arriving after the recipe stopped settles
    /// under the stop that orphaned it.
    pub last_stop: Option<Wasted>,
    pub last_probe_at: Option<i64>,
    /// The probes sent on `counters_day`, the UTC day both counts are for.
    pub probes_today: u64,
    /// The requests credited as kept warm on `counters_day`.
    pub credited_today: u64,
    pub counters_day: i64,
}

impl AgentWarming {
    pub(crate) fn new(now: i64, timing: KeepaliveTiming) -> AgentWarming {
        AgentWarming {
            key_generation: 0,
            idle_episode: None,
            idle_window_floor: None,
            last_prefix: None,
            last_prompt_tokens: None,
            current: None,
            aside: None,
            compaction: None,
            body_dirty: false,
            body_saved: true,
            expires: None,
            jitter_frac: 0.0,
            retry_at: None,
            timing,
            cache_started: now,
            ttl_ms: 0,
            cold: 0,
            failures: 0,
            account: None,
            account_incarnation: None,
            pool_id: String::new(),
            canonical_model: String::new(),
            entrypoint: None,
            reason: Reason::AwaitingRequest,
            last_guard: None,
            hold_counted: false,
            paused: false,
            in_flight: false,
            probes: 0,
            usage: ProbeUsage::default(),
            unanswered: Vec::new(),
            kept: Vec::new(),
            last_real_at: None,
            last_answered_at: None,
            last_stop: None,
            last_probe_at: None,
            probes_today: 0,
            credited_today: 0,
            counters_day: utc_day(now),
        }
    }

    /// Moves every model name the agent holds that `canonical` dates, and answers whether any moved: the recipe's
    /// own, the one its open observation took from the recipe before it, an open compaction's own and the one it took,
    /// and its kept and unanswered probes', which an agent keeps after its recipe ended.
    pub(crate) fn follow_resolutions(&mut self, canonical: &HashMap<String, String>) -> bool {
        let follow = |model: &mut String| match canonical.get(model.as_str()) {
            Some(dated) => {
                *model = dated.clone();
                true
            }
            None => false,
        };
        let mut moved = follow(&mut self.canonical_model);
        if let Some(prior) = self.current.as_mut().and_then(|o| o.prior.as_mut()) {
            moved |= follow(&mut prior.model);
        }
        // The completion credits through the compaction's `prior` and settles the probes against its own model, so
        // both follow the probes' move, or a credit would sit beside a cold return of the probe that earned it.
        if let Some(compaction) = self.compaction.as_mut() {
            moved |= follow(&mut compaction.canonical_model);
            if let Some(prior) = compaction.prior.as_mut() {
                moved |= follow(&mut prior.model);
            }
        }
        for kept in &mut self.kept {
            moved |= follow(&mut kept.canonical_model);
        }
        for probe in &mut self.unanswered {
            moved |= follow(&mut probe.model);
        }
        moved
    }

    /// Every probe the agent paid for that no request answered, the unanswered ones and then the kept ones, settled
    /// under `label`.
    pub(crate) fn settle_all(&mut self, key: &SessionKey, label: Wasted) -> Vec<Settlement> {
        self.take_unanswered()
            .into_iter()
            .chain(std::mem::take(&mut self.kept))
            .map(|kept| kept.settle(key, label))
            .collect()
    }

    /// The unanswered probes, taken off the agent as kept ones.
    pub(crate) fn take_unanswered(&mut self) -> Vec<Kept> {
        let unanswered = std::mem::take(&mut self.unanswered);
        unanswered
            .into_iter()
            .map(|probe| Kept::of(self, probe))
            .collect()
    }

    /// Whether the agent holds anything that a stop of the warming ends: an observation, a recipe set aside, or a
    /// probe that waits for its answer or its settlement.
    pub(crate) fn holds_warming_work(&self) -> bool {
        self.current.is_some()
            || self.aside.is_some()
            || !self.unanswered.is_empty()
            || !self.kept.is_empty()
    }

    /// Whether the current recipe runs in fast mode, false while the agent holds none.
    pub(crate) fn fast(&self) -> bool {
        self.current.as_ref().is_some_and(|o| o.recipe.fast)
    }

    /// The names `follow_resolutions` reads on an agent with no compaction open, as an arrival leaves it once
    /// `end_compaction` ran: the ones the arrival looks up in the catalog's acknowledged resolution.
    pub(crate) fn held_models(&self) -> BTreeSet<&str> {
        let mut models = BTreeSet::from([self.canonical_model.as_str()]);
        models.extend(
            self.current
                .as_ref()
                .and_then(|o| o.prior.as_ref())
                .map(|prior| prior.model.as_str()),
        );
        models.extend(self.kept.iter().map(|kept| kept.canonical_model.as_str()));
        models.extend(self.unanswered.iter().map(|probe| probe.model.as_str()));
        models
    }

    /// When the next probe is due: the entry's expiry less the timing's lead and this schedule's share of the
    /// jitter, or the outstanding backoff, whichever is later. `None` while nothing is armed.
    pub(crate) fn due_at(&self) -> Option<i64> {
        if self.paused {
            return None;
        }
        let from_expiry = self.expires.map(|expires| {
            let jitter = (self.timing.jitter_ms(self.ttl_ms) as f64 * self.jitter_frac) as i64;
            expires - self.timing.lead_ms(self.ttl_ms) - jitter
        });
        from_expiry.max(self.retry_at)
    }

    /// The due time a reader is shown: none while the recipe sits under one of the two holds placed ahead of the
    /// gate, `horizon` and a disabled pool, which withhold every probe until they end or the entry lapses. The
    /// scheduler reads `due_at`, so a withheld probe is still counted once.
    pub(crate) fn next_probe_at(&self) -> Option<i64> {
        if matches!(self.reason, Reason::Horizon | Reason::PoolDisabled) {
            return None;
        }
        self.due_at()
    }

    /// Every close of an idle window: the window start in force at the close becomes the floor the next window's start
    /// has to pass. A prompt answered by a local command moves the start without closing the window, so the
    /// episode's own `started` can lie behind it. A closer that moves the start calls this first.
    pub(crate) fn close_idle_episode(&mut self, agent: &Agent) -> Option<IdleEpisode> {
        let episode = self.idle_episode.take()?;
        self.idle_window_floor = Some(episode.started.max(agent.idle_window_start()));
        Some(episode)
    }

    /// The expiry stays, so the lapse still drops the prompt when the entry goes cold, as it does for a held recipe.
    pub(crate) fn pause(&mut self) {
        self.paused = true;
        self.retry_at = None;
    }

    /// Starts both day counts over when `now` falls on a later UTC day than the one they count. Only the two events
    /// they count call it, so no tick resets them: `counts_today` reads a passed day as zero instead.
    fn roll_counters_day(&mut self, now: i64) {
        let day = utc_day(now);
        if self.counters_day != day {
            self.counters_day = day;
            self.probes_today = 0;
            self.credited_today = 0;
        }
    }

    pub(crate) fn count_probe(&mut self, now: i64) {
        self.roll_counters_day(now);
        self.last_probe_at = Some(now);
        self.probes_today += 1;
    }

    pub(crate) fn count_credit(&mut self, now: i64) {
        self.roll_counters_day(now);
        self.credited_today += 1;
    }

    /// The day's probes and credits as of `now`, zero once the UTC day they count has passed: a recipe that neither
    /// probed nor earned a credit since midnight reads the new day without waiting for either event.
    pub(crate) fn counts_today(&self, now: i64) -> (u64, u64) {
        if self.counters_day == utc_day(now) {
            (self.probes_today, self.credited_today)
        } else {
            (0, 0)
        }
    }
}

/// The warming's record of the agent `agent_id` of the session `key`, borrowing the warming's map alone, so a caller
/// may hold the session engine's record of the same agent beside it.
pub(crate) fn warming_mut<'a>(
    agents: &'a mut BTreeMap<SessionKey, BTreeMap<String, AgentWarming>>,
    key: &SessionKey,
    agent_id: &str,
) -> Option<&'a mut AgentWarming> {
    agents.get_mut(key)?.get_mut(agent_id)
}

/// `warming_mut`'s shared twin.
pub(crate) fn warming_ref<'a>(
    agents: &'a BTreeMap<SessionKey, BTreeMap<String, AgentWarming>>,
    key: &SessionKey,
    agent_id: &str,
) -> Option<&'a AgentWarming> {
    agents.get(key)?.get(agent_id)
}
