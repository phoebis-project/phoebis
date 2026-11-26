//! The engine: every session the companion registered or a request revealed, every agent's recipe, and the
//! counters behind the views. One mutex, as the Go engine had, held for microseconds at a time. Nothing under
//! it does I/O.

pub(crate) mod config;
pub(crate) mod persist;
pub(crate) mod report;
pub(crate) mod view;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{
    spawn_subscriber, AccountSelected, Bus, Delivery, Event as BusEvent, Phase, RequestComplete,
    RequestObserved, ResponseChunk, ResponseHead,
};
use phoebis_ledger::{Ledger, Metrics, PlanTables, Pricing};
use phoebis_oauth::Clock;
use phoebis_relay::Relay;
use phoebis_routing::{
    budget_deadline, listing, Holder, MemberSpend, RegistrySnapshot, RouterHandle,
};
use phoebis_store::{AccountKind, Store};
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::engine::persist::Dirty;
use crate::sessions::config::SessionCaps;
use crate::sessions::state::{
    agent_ref, Arrived, ParkPassed, SessionKey, SessionsState, Slot, AGENT_HELD, MAIN_AGENT,
};
use crate::sessions::view::SessionCounts;
use crate::warming::agent::warming_mut;
use crate::warming::breakeven::{CeilingInputs, FactorRows};
use crate::warming::causes::{Reason, Wasted};
use crate::warming::config::{KeepaliveLimits, KeepalivePolicy, KeepaliveTiming, MAX_WORKERS};
use crate::warming::gate::{
    ceilings_for, gate, pool_policies, refresh_policy, Gate, RouterView, Verdict,
};
use crate::warming::guard::{class_guard, Guard};
use crate::warming::idle::{utc_day, IdleReturns};
use crate::warming::observation::{
    ChainMark, Credit, Skipped, Unanswered, CHAIN_CONTRADICTION_COVERAGE,
};
use crate::warming::probe::{ceiling, run_job, Exchange, ProbeJob, ProbeRecord};
use crate::warming::recipe::make_recipe;
use crate::warming::requests::reported_prompt;
use crate::warming::savings::{AttemptFunding, SavingsUnit};
use crate::warming::state::{Change, Changes, WarmingState};
use crate::warming::usage::ProbeUsage;
use crate::warming::view::WarmingCounts;

const LOCK_POISONED: &str = "keepalive engine poisoned by a prior panic";

pub struct EngineParts {
    pub bus: Bus,
    pub clock: Clock,
    /// The overage predicate's fallback: how long evidence naming no reset stands past its observation, the
    /// same value the relay's `Limits` gets, so the probe gate and the selector read one account the same way.
    pub cooldown_fallback: Duration,
    pub ledger: Ledger,
    pub limits: KeepaliveLimits,
    /// The relay's per-body bound: the largest body a recipe may retain, so every request the relay accepts is
    /// retainable.
    pub max_request_bytes: usize,
    pub metrics: Arc<Metrics>,
    pub pricing: Arc<Pricing>,
    pub relay: Arc<Relay>,
    pub router: RouterHandle,
    pub session_caps: SessionCaps,
    pub session_expiry: Duration,
    /// The registry's own store, read for the sessions and recipes a restart resumes and for the key the companion's
    /// account pseudonyms are digested under.
    pub store: Arc<Store>,
    /// The subscription cost model, for a probe on a plan-funded attempt.
    pub tables: Arc<PlanTables>,
    /// The schedule's leads, jitters and retry limits: `KeepaliveTiming::for_start()` in the binary, and a test's
    /// own numbers where it asserts exact times.
    pub timing: KeepaliveTiming,
}

#[derive(Debug)]
pub struct EngineStats {
    /// The request events (a selection, a head, a chunk, a completion, a new request) and the reports refused because
    /// they arrived after the shutdown's quiesce, so the state the last save holds is the state at the quiesce.
    pub late_updates: u64,
    pub sessions: SessionCounts,
    pub warming: WarmingCounts,
}

pub(crate) struct Inner {
    pub sessions: SessionsState,
    pub warming: WarmingState,
    pub closed: bool,
    /// The rows changed or removed since their last acknowledged save, each under the revision of its latest change.
    pub dirty: Dirty,
    /// Bumped by every touch, so a save that lands after a newer change still knows which change it carried.
    pub revision: u64,
    /// Whether the last save failed, so a failing registry is logged once per streak rather than every flush.
    pub persist_failing: bool,
    /// The request events and reports refused after the quiesce.
    pub late_updates: u64,
}

pub struct Engine {
    pub(crate) inner: Mutex<Inner>,
    pub(crate) parts: EngineParts,
    pub(crate) cancel: CancellationToken,
    pub(crate) workers: TaskTracker,
    /// Orders the flushes: a snapshot, its save and its acknowledgment run under it whole, so two flushes never
    /// overlap and an older batch never reaches the store after a newer one.
    pub(crate) flush_lock: Mutex<()>,
    /// Wakes the save worker. It holds at most one stored wakeup, which is what joins a burst of requests into one
    /// save.
    pub(crate) save_wanted: Arc<tokio::sync::Notify>,
    /// Set once the save worker runs.
    pub(crate) save_worker: std::sync::OnceLock<()>,
    #[cfg(any(test, feature = "debug-hooks"))]
    pub(crate) hooks: DebugHooks,
    /// Events a gap left queued behind it, counted down as they are applied. What arrives inside that backlog cannot
    /// be trusted: the terminal event of any request among them may be one the gap dropped. Apart from the engine's
    /// lock, so an event that takes no lock of its own counts down without one.
    gap_backlog: AtomicUsize,
}

#[cfg(any(test, feature = "debug-hooks"))]
#[derive(Default)]
pub(crate) struct DebugHooks {
    /// How many flushes have entered, counted before the flush lock, so a test can hold a save and know a second
    /// flush is queued behind it.
    pub(crate) persist_entries: std::sync::atomic::AtomicU64,
    /// Woken as `persist_entries` or `persist_snapshots` rises, which is what a test's wait on them waits on.
    pub(crate) persist_progress: tokio::sync::Notify,
    /// How many flushes have read their batch, for a test to change a row only once the save it races has read it.
    pub(crate) persist_snapshots: std::sync::atomic::AtomicU64,
    /// How many times `step` has actually asked the router, for a test to hold against one ask per due tick.
    router_view_calls: std::sync::atomic::AtomicU64,
}

#[cfg(any(test, feature = "debug-hooks"))]
impl DebugHooks {
    pub(crate) fn raise(&self, count: &std::sync::atomic::AtomicU64) {
        count.fetch_add(1, Ordering::SeqCst);
        self.persist_progress.notify_waiters();
    }
}

impl Engine {
    pub fn new(parts: EngineParts) -> Arc<Engine> {
        // Every read of a stored policy the bounds now refuse answers keepalive off. The operator learns which pool
        // and why once, here, rather than from a tick that reads it every second.
        for pool in parts.router.snapshot().pools() {
            if let Err(refusal) = KeepalivePolicy::read(&pool.keepalive_json) {
                tracing::warn!(
                    pool = %pool.id,
                    %refusal,
                    "a pool's stored keepalive policy is refused and reads as keepalive off"
                );
            }
        }
        Arc::new(Engine {
            inner: Mutex::new(Inner::empty(
                parts.timing,
                parts.limits.clone(),
                parts.session_caps.clone(),
                parts.session_expiry.as_millis() as i64,
            )),
            parts,
            cancel: CancellationToken::new(),
            workers: TaskTracker::new(),
            flush_lock: Mutex::new(()),
            save_wanted: Arc::new(tokio::sync::Notify::new()),
            save_worker: std::sync::OnceLock::new(),
            #[cfg(any(test, feature = "debug-hooks"))]
            hooks: DebugHooks::default(),
            gap_backlog: AtomicUsize::new(0),
        })
    }

    /// Compiled only under the `debug-hooks` feature, which a dependent's dev-dependency turns on, so a
    /// release build carries no way to count the scheduler's own router traffic.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn router_views(&self) -> u64 {
        self.hooks
            .router_view_calls
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    pub fn now(&self) -> i64 {
        (self.parts.clock)()
    }

    pub(crate) fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().expect(LOCK_POISONED)
    }

    /// How many times each reason was written since the start, an agent's own or one that no agent's reason takes.
    /// Compiled only under the `debug-hooks` feature, which the crate's own dev-dependency turns on for its suites:
    /// no view reads the count, so a release build keeps none.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn reasons(&self) -> BTreeMap<Reason, u64> {
        self.lock().warming.reasons.clone()
    }

    /// The counters per pool id summed over the UTC day of `now` and the `days - 1` before it, for the overview, so
    /// the day fold stays the engine's.
    pub fn idle_returns_over(&self, now: i64, days: u32) -> BTreeMap<String, IdleReturns> {
        let last = utc_day(now);
        let first = last - i64::from(days) + 1;
        self.lock()
            .warming
            .idle_returns
            .iter()
            .map(|(pool, by_day)| {
                let mut sum = IdleReturns::default();
                for counters in by_day.range(first..last + 1).map(|(_, counters)| counters) {
                    for (total, bucket) in sum.buckets.iter_mut().zip(&counters.buckets) {
                        total.reached += bucket.reached;
                        total.returned += bucket.returned;
                    }
                }
                (pool.clone(), sum)
            })
            .collect()
    }

    /// The byte cap that the bootstrap file's `keepalive` block sets, which the sessions overview answers beside its
    /// retained bytes.
    pub fn limits(&self) -> &KeepaliveLimits {
        &self.parts.limits
    }

    /// The two caps that the bootstrap file's `sessions` block sets, which the sessions overview answers beside its
    /// counts.
    pub fn session_caps(&self) -> &SessionCaps {
        &self.parts.session_caps
    }

    /// The pools whose policy warms caches, read off the router's snapshot as the scheduler reads a pool's policy
    /// (`pool_policies`).
    pub fn warming(&self) -> BTreeSet<String> {
        pool_policies(&self.parts.router.snapshot())
            .into_iter()
            .filter(|(_, policy)| policy.enabled)
            .map(|(pool, _)| pool)
            .collect()
    }

    pub fn stats(&self) -> EngineStats {
        let inner = self.lock();
        EngineStats {
            late_updates: inner.late_updates,
            sessions: inner.sessions.counts(),
            warming: inner.warming.counts(),
        }
    }

    /// The inputs every pool's ceilings read, at this instant. The router's catalog names the dated id each undated
    /// dollar row resolves to, and a router that cannot answer leaves every row judged under its own id.
    pub async fn ceiling_inputs(&self) -> CeilingInputs {
        let now = self.now();
        let revisions = self.parts.pricing.revisions();
        let rows = FactorRows::of(revisions.in_force(now), &self.parts.tables, now);
        let undated: Vec<String> = rows
            .dollar_models()
            .filter(|model| listing::undated(model).is_none())
            .map(str::to_string)
            .collect();
        let dated = self.dated_names(undated).await;
        CeilingInputs {
            rows,
            dated,
            timing: self.parts.timing,
        }
    }

    /// Each of `models` the router's catalog resolves to another name, with that name. A router that cannot answer
    /// leaves every name as it is.
    async fn dated_names(&self, models: Vec<String>) -> HashMap<String, String> {
        self.parts
            .router
            .canonicals(models.clone())
            .await
            .map(|answered| {
                models
                    .into_iter()
                    .zip(answered)
                    .filter(|(model, dated)| model != dated)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The lease and expiry sweep. Every entry point runs it first, so a session is never read as live past
    /// its lease by a caller that happened to arrive before the ticker.
    pub fn expire(&self) {
        let now = self.now();
        self.lock().sweep(now);
        self.flush();
    }

    pub fn close(&self) {
        let mut inner = self.lock();
        let inner = &mut *inner;
        inner.closed = true;
        let keys: Vec<SessionKey> = inner.sessions.entries.keys().cloned().collect();
        for key in keys {
            inner.warm(|w, s| w.discard_session(s, &key, Reason::Reset, None));
        }
        inner.sessions.clear();
        inner.warming.clear();
        self.cancel.cancel();
    }
}

impl Inner {
    /// A fresh engine's own state, and the fixture a unit test builds one agent onto: with the constructors of the two
    /// states, the one place every field is named, so a field one of the three structs gains needs no second literal to
    /// keep in step.
    pub(crate) fn empty(
        timing: KeepaliveTiming,
        limits: KeepaliveLimits,
        session_caps: SessionCaps,
        session_expiry_ms: i64,
    ) -> Inner {
        Inner {
            sessions: SessionsState::new(
                session_caps.max_agents,
                session_caps.max_sessions,
                session_expiry_ms,
            ),
            warming: WarmingState::new(timing, limits.max_retained_bytes),
            closed: false,
            dirty: Dirty::default(),
            revision: 0,
            persist_failing: false,
            late_updates: 0,
        }
    }

    /// Applies what a warming call left, in the order of the call's statements: an end moves the agent's request
    /// generation in the session engine, and a changed row is marked for the next save.
    pub(crate) fn apply_changes(&mut self, changes: Changes) {
        for change in changes.0 {
            match change {
                Change::Ended { key, agent_id } => self.sessions.end_observation(&key, &agent_id),
                Change::Agent { key, agent_id } => self.touch_agent(&key, &agent_id),
                Change::Pool(pool_id) => self.touch_pool(&pool_id),
            }
        }
    }

    /// The lease and expiry sweep: the released records past their leave, the leases that lapsed and the sessions past
    /// the expiry.
    pub(crate) fn sweep(&mut self, now: i64) {
        // A record whose probe is still in flight waits for the result, which settles under the release's label, and
        // the next sweep takes it.
        for (key, agent_id) in self.sessions.released_due(now) {
            if self.warming.in_flight(&key, &agent_id) {
                continue;
            }
            self.forget_agent(&key, &agent_id);
            self.sessions.drop_agent(&key, &agent_id);
            self.warming.drop_record(&key, &agent_id);
            self.touch_session(&key);
        }
        for key in self.sessions.lapse_leases(now) {
            self.warming.lease_lapsed(&self.sessions, &key);
            self.touch_session(&key);
            self.touch_agent(&key, MAIN_AGENT);
        }
        for key in self.sessions.expired(now) {
            self.remove_session(&key, Reason::SessionExpired);
        }
    }

    /// Ends the session under `reason`: its agents discarded, their probes settled as `session_ended`, its rows filed
    /// for removal and its entry gone.
    fn remove_session(&mut self, key: &SessionKey, reason: Reason) {
        self.warm(|w, s| w.discard_session(s, key, reason, Some(Wasted::SessionEnded)));
        self.forget_session(key);
        self.warming.drop_records(key);
        self.sessions.remove_session(key);
    }

    /// Ends every session whose key the registry no longer holds under the generation that registered it: a
    /// deletion, or the same id created again, takes the previous holder's sessions and recipes with it rather
    /// than leaving them to the session expiry. The filed sets of such a generation leave behind them, each found by
    /// its own key, since a set can be filed for a session that the map holds no record of.
    pub(crate) fn drop_sessions_of_dead_keys(&mut self, snapshot: &RegistrySnapshot) {
        for key in self.sessions.sessions_of_dead_keys(snapshot) {
            self.remove_session(&key, Reason::SessionEnded);
        }
        self.forget_facts_of_dead_keys(snapshot);
    }
}

/// The engine's bus task. Every event is handed to `observe` under the lock. Nothing here awaits while holding
/// it, so a slow registry read never stalls the queue. The bus drops for a subscriber that falls behind and
/// counts it. A rise in that counter means events this task never saw, so every observation still open is
/// discarded before the next event is applied.
pub fn spawn_observer(engine: Arc<Engine>) -> tokio::task::JoinHandle<()> {
    // Best-effort: a drop reads as a gap below, which discards the observations it could have misled. The queue holds
    // the kinds the engine reads, so a burst of feature calls, which move nothing it keeps warm, raises no gap.
    let subscriber =
        engine
            .parts
            .bus
            .subscribe_filtered("keepalive", Delivery::BestEffort, |event| {
                !matches!(event, BusEvent::FeatureCall(_))
            });
    spawn_subscriber(subscriber, move |mut subscriber| async move {
        let mut seen_dropped = 0u64;
        while let Some(event) = subscriber.events.recv().await {
            let dropped = subscriber
                .stats
                .dropped
                .load(std::sync::atomic::Ordering::Relaxed);
            if dropped > seen_dropped {
                // What is already queued behind the gap is the backlog the engine has to distrust, so its
                // depth is read here, where the receiver is.
                engine.bus_gap(dropped - seen_dropped, subscriber.events.len());
                seen_dropped = dropped;
            }
            engine.observe(event);
        }
    })
}

impl Engine {
    pub fn observe(&self, event: BusEvent) {
        // Each arm takes the lock and gives it back before returning. The drain belongs here rather than in
        // the arms: an observation sweeps the lapsed sessions, and the settlements that sweep leaves have to
        // reach the ledger stamped with this event rather than with whatever call drains next.
        // A chunk, a head and a selection push nothing to settle or to skip, so they flush nothing.
        let flushes = matches!(
            event,
            BusEvent::RequestObserved(_) | BusEvent::RequestComplete(_)
        );
        match event {
            BusEvent::RequestObserved(observed) => {
                let catalog = self.parts.relay.catalog();
                self.on_observed(observed, |spelling| catalog.lookup(spelling));
            }
            BusEvent::AccountSelected(selected) => self.on_selected(selected),
            BusEvent::ResponseHead(head) => self.on_head(head),
            BusEvent::ResponseChunk(chunk) => self.on_chunk(chunk),
            BusEvent::RequestComplete(complete) => self.on_complete(complete),
            // A feature call carries no session and no model, so it moves nothing the engine keeps warm.
            BusEvent::FeatureCall(_) => {}
        }
        self.counted_down();
        if flushes {
            self.flush();
        }
    }

    /// One event of the backlog applied. The count is read before the event's arm rather than after, so the event a
    /// gap was noticed on is itself inside its own backlog.
    fn counted_down(&self) {
        let _ = self
            .gap_backlog
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1));
    }

    /// Events were lost between the last one seen and this one. Whatever was still open cannot be completed
    /// with confidence: the lost events may have been the completion, or a failover onto another account. An
    /// armed recipe's evidence was complete before the gap and stays.
    pub fn bus_gap(&self, dropped: u64, backlog: usize) {
        let mut inner = self.lock();
        let inner = &mut *inner;
        self.gap_backlog.store(backlog, Ordering::SeqCst);
        tracing::warn!(
            dropped,
            "the keepalive subscriber fell behind the bus, so the pending observations were discarded"
        );
        inner.warm(|w, s| w.bus_gap(s, self.now()));
    }

    fn on_observed(&self, observed: RequestObserved, resolve: impl Fn(&str) -> Option<String>) {
        // Only an inference request says anything about the cache entry a recipe is for. Token counting
        // carries the same session and agent headers and writes no cache, so it must reach nothing below:
        // not the recipe, not the schedule, and not the request time the kept-warm gap is measured from.
        if observed.origin == phoebis_bus::Origin::Keepalive
            || observed.kind != phoebis_bus::Kind::Inference
        {
            return;
        }
        let (Some(key_id), Some(session_id), Some(agent_id)) = (
            observed.key_id.clone(),
            observed.session_id.clone(),
            observed.agent_id.clone(),
        ) else {
            return;
        };
        let now = self.now();
        let key: SessionKey = (key_id, session_id);
        // Read ahead of the body's parse, so a request on a pool with the keepalive off or from an opted-out key keeps
        // no body. A key the snapshot lacks reads the default policy, which is off.
        let arrival_refusal = {
            let snapshot = self.parts.router.snapshot();
            let key_row = snapshot.key(&key.0);
            let enabled = key_row
                .and_then(|row| snapshot.pool(&row.pool_id))
                .is_some_and(|pool| KeepalivePolicy::from_json(&pool.keepalive_json).enabled);
            if !enabled {
                Some(Reason::KeepaliveOff)
            } else if key_row.is_some_and(|row| !row.companion_permissions.keepalive) {
                Some(Reason::OptedOut)
            } else {
                None
            }
        };
        // The body's parse and edits run apart from the lock, which the bus observer, the tick and the companion
        // route all take, for a request on an agent the engine holds, of a session whose own option holds the warming
        // on, and of a class that can arm a recipe: a compaction stores none. Every check below runs again under the
        // lock, which builds the recipe itself where none was built here.
        let (held, session_off) = {
            let inner = self.lock();
            (
                !inner.closed && agent_ref(&inner.sessions.entries, &key, &agent_id).is_some(),
                inner
                    .sessions
                    .entries
                    .get(&key)
                    .is_some_and(|session| !session.cache_warming),
            )
        };
        let build = || {
            make_recipe(
                &observed.headers,
                &observed.body,
                &observed.model,
                self.parts.max_request_bytes,
            )
        };
        let prebuilt = (held
            && arrival_refusal.is_none()
            && !session_off
            && observed
                .client
                .request_class
                .as_deref()
                .is_none_or(|class| class_guard(class) == Guard::Rearm))
        .then(build);
        let mut inner = self.lock();
        let inner = &mut *inner;
        if inner.closed {
            // A request on a session the engine holds is one the last save can no longer carry.
            if inner.sessions.entries.contains_key(&key) {
                inner.late_updates += 1;
            }
            return;
        }
        inner.sweep(now);
        if !inner
            .sessions
            .arrival_check(&key, observed.key_generation, &agent_id)
        {
            return;
        }
        if inner.warming.follows(&key, &agent_id, &observed.request_id) {
            return;
        }
        if !inner
            .sessions
            .instance_check(&key, observed.instance_id.as_deref(), now)
        {
            #[cfg(any(test, feature = "debug-hooks"))]
            inner.warming.count(Reason::InstanceMismatch);
            return;
        }
        let Some(arriving) =
            inner.warm_with(|w, s| w.arrival_guard(s, &key, &agent_id, &observed, &resolve, now))
        else {
            return;
        };
        let poisoned = self.gap_backlog.load(Ordering::SeqCst) > 0;
        // The recipe set aside waits for this request's completion, so each return below that takes no observation
        // drops it: nothing would complete to restore it.
        inner.warm(|w, s| w.set_aside(s, &key, &agent_id, now));
        let slot = inner.sessions.arrival_slot(&key, now);
        inner.touch_session(&key);
        let refused = match slot {
            Slot::OverSessions => Some(Reason::SessionLimit),
            Slot::OverAgents => Some(Reason::AgentLimit),
            Slot::Held | Slot::Taken => None,
        };
        if let Some(reason) = refused {
            // The agent saw a real request, so the view lists its line with the cause, and `slotted` stays false,
            // so the session holds no slot.
            inner
                .warming
                .arrival_seen(&key, &agent_id, &observed.canonical_model, now);
            inner.warm(|w, s| w.arrival_refuse(s, &key, &agent_id, reason));
            return;
        }
        if inner.sessions.arrival_busy(&key, &agent_id, now) == Arrived::Unknown {
            inner.warm(|w, s| w.arrival_refuse(s, &key, &agent_id, Reason::AgentUnknown));
            return;
        }
        inner.warm(|w, s| {
            w.arrival_arm(
                s,
                &key,
                &agent_id,
                &observed,
                now,
                arrival_refusal,
                move || prebuilt.unwrap_or_else(build),
                arriving,
                poisoned,
            )
        });
    }

    fn on_selected(&self, selected: AccountSelected) {
        let funding = AttemptFunding {
            kind: AccountKind::parse(&selected.account_kind),
            usage_based: selected.usage_based,
            in_overage: false,
        };
        let snapshot = self.parts.router.snapshot();
        let mut inner = self.lock();
        let inner = &mut *inner;
        match inner.route(&selected.request_id) {
            None => {}
            Some(Followed::Compaction) => {
                inner
                    .warming
                    .on_compaction_selected(&inner.sessions, selected, funding);
            }
            Some(Followed::Agent(key, agent_id)) => {
                inner.warm(|w, s| w.on_selected(s, &key, &agent_id, selected, funding, &snapshot))
            }
        }
    }

    fn on_head(&self, head: ResponseHead) {
        let in_overage = phoebis_relay::UnifiedHeaders::parse_list(&head.headers)
            .and_then(|unified| unified.overage_in_use)
            == Some(true);
        let mut inner = self.lock();
        let inner = &mut *inner;
        match inner.route(&head.request_id) {
            None => {}
            Some(Followed::Compaction) => {
                inner
                    .warming
                    .on_compaction_head(&inner.sessions, &head, in_overage);
            }
            Some(Followed::Agent(key, agent_id)) => {
                inner.warm(|w, s| w.on_head(s, &key, &agent_id, &head, in_overage));
            }
        }
    }

    fn on_chunk(&self, chunk: ResponseChunk) {
        let usage = match &chunk.phase {
            Some(Phase::MessageStart { usage }) | Some(Phase::MessageDelta { usage }) => {
                ProbeUsage::from_bus(usage)
            }
            _ => return,
        };
        let mut inner = self.lock();
        let inner = &mut *inner;
        match inner.route(&chunk.request_id) {
            None => {}
            Some(Followed::Compaction) => {
                inner
                    .warming
                    .on_compaction_chunk(&inner.sessions, &chunk, usage);
            }
            Some(Followed::Agent(key, agent_id)) => {
                inner.warm(|w, s| w.on_chunk(s, &key, &agent_id, &chunk, usage));
            }
        }
    }

    fn on_complete(&self, complete: RequestComplete) {
        let now = self.now();
        let mut guard = self.lock();
        let inner = &mut *guard;
        if !inner.warming.compaction_open(&complete.request_id) {
            if let Some(mark) = inner.warming.take_chain_mark(&complete.request_id) {
                self.read_chain_mark(inner, mark, &complete, now);
            }
        }
        let credit = match inner.route(&complete.request_id) {
            None => return,
            Some(Followed::Compaction) => {
                inner.warm_with(|w, s| w.complete_compaction(s, &complete, now))
            }
            Some(Followed::Agent(key, agent_id)) => {
                inner.warm_with(|w, s| w.on_complete(s, &key, &agent_id, &complete, now))
            }
        };
        self.book_credit(guard, credit, &complete);
    }

    /// The chain watch: a marked request whose completion read the cache at or above `CHAIN_CONTRADICTION_COVERAGE` of
    /// the previous main prompt contradicts the router's chain model, since the upstream read as a continuation what
    /// the chain called a new prefix. Every contradiction counts, one line per client version per hour names it, and
    /// the completion's own prompt becomes the one the next mark reads.
    fn read_chain_mark(
        &self,
        inner: &mut Inner,
        mark: ChainMark,
        complete: &RequestComplete,
        now: i64,
    ) {
        let read = complete
            .usage
            .as_ref()
            .and_then(|u| u.cache_read_tokens)
            .unwrap_or(0);
        if read as f64 >= mark.previous_prompt as f64 * CHAIN_CONTRADICTION_COVERAGE {
            let version = complete.client_version.as_deref().unwrap_or("unknown");
            self.parts.metrics.observe_chain_contradiction(version);
            if inner.warming.note_chain_warning(version, now) {
                tracing::warn!(
                    client_version = %version,
                    session = %mark.session.1,
                    cache_read_tokens = read,
                    previous_prompt_tokens = mark.previous_prompt,
                    "a refreshing request's chain holds none of the anchor's boundaries while its completion read \
                     the cache as a continuation would"
                );
            }
        }
        // After the quiesce the last save holds the agent as it stood then.
        if inner.closed {
            return;
        }
        if let Some(prompt) = reported_prompt(complete.usage.as_ref()) {
            inner
                .warming
                .set_last_prompt(&inner.sessions, &mark.session, prompt);
            inner.touch_agent(&mark.session, MAIN_AGENT);
        }
    }

    /// Books the credit a completion earned and releases the lock on the way.
    fn book_credit(
        &self,
        inner: std::sync::MutexGuard<'_, Inner>,
        credit: Option<Credit>,
        complete: &RequestComplete,
    ) {
        drop(inner);
        let Some(credit) = credit else {
            return;
        };
        // The tier the router's snapshot prices the serving account by, the one the request row files under, so a
        // stored term the router has not loaded yet leaves the credit beside its row.
        let snapshot = self.parts.router.snapshot();
        let tier = credit
            .account
            .as_deref()
            .and_then(|account| snapshot.priced_tier(account))
            .map(String::from);
        // The band the request's own row keys, read off its prompt as the ledger reads the serving attempt's.
        // A completion with no usage reads as a prompt of 0, which every row bands as standard.
        let band = self.parts.pricing.revisions().in_force(credit.at).band_of(
            &credit.model,
            reported_prompt(complete.usage.as_ref()).unwrap_or(0),
        );
        self.record_kept_warm(&credit, &complete.request_id, tier, band);
    }
}

/// What a request event's request id names: a compaction open on an agent, or the agent whose observation follows it.
enum Followed {
    Compaction,
    Agent(SessionKey, String),
}

impl Inner {
    /// Whether the engine is closed, counting the event that meets it as a late update.
    fn quiesced(&mut self) -> bool {
        if self.closed {
            self.late_updates += 1;
        }
        self.closed
    }

    /// Where a request event on `request_id` goes: the compaction open under it, read ahead of the agent's observation
    /// so the observation's paths never see a compaction, else the agent following it. `None` for a request neither
    /// follows and for any once the engine is closed. After the quiesce the last save holds each agent as it stood
    /// then, so a later event is refused and counted: a completion earns no credit and arms nothing, and the relay has
    /// answered the client either way.
    fn route(&mut self, request_id: &str) -> Option<Followed> {
        let followed = if self.warming.compaction_open(request_id) {
            Followed::Compaction
        } else {
            let (key, agent_id) = self.warming.follower(request_id)?;
            Followed::Agent(key, agent_id)
        };
        if self.quiesced() {
            return None;
        }
        Some(followed)
    }

    /// A warming call with the changes it leaves applied. The session state goes over shared, since no function of the
    /// warming writes it.
    fn warm(&mut self, call: impl FnOnce(&mut WarmingState, &SessionsState) -> Changes) {
        let changes = call(&mut self.warming, &self.sessions);
        self.apply_changes(changes);
    }

    /// `warm` for a call that answers a value beside its changes.
    fn warm_with<T>(
        &mut self,
        call: impl FnOnce(&mut WarmingState, &SessionsState) -> (Changes, T),
    ) -> T {
        let (changes, value) = call(&mut self.warming, &self.sessions);
        self.apply_changes(changes);
        value
    }
}

impl Engine {
    /// One tick: what is due runs, up to the worker bound. Called by the production ticker every second and by
    /// the tests directly.
    pub async fn step(self: &Arc<Self>) {
        let now = self.now();
        // The six asks behind `router_view` (canonicals, pins, cooldowns, overages, member spends, probe holds) are a
        // round trip apiece, and a tick with no retained recipe pays none of them. `drop_sessions_of_dead_keys` reads
        // the snapshot alone, which `RouterHandle::snapshot` answers without a command.
        let snapshot = self.parts.router.snapshot();
        let retained_models = {
            let mut inner = self.lock();
            let inner = &mut *inner;
            if inner.closed {
                return;
            }
            inner.sweep(now);
            inner.drop_sessions_of_dead_keys(&snapshot);
            inner.warm(|w, s| w.advance_idle_episodes(s, &snapshot, now));
            // A probe the ceiling wrote off keeps its entry alive for a turn that may still read it, so the write-off
            // waits for the entry's own end, whether or not any recipe is still retained.
            inner.warm(|w, s| w.settle_lapsed_pending(s, now));
            inner
                .warming
                .anything_retained(&inner.sessions)
                .then(|| inner.warming.retained_models(&inner.sessions))
        };
        let Some(retained_models) = retained_models else {
            return;
        };
        let view = self.router_view(now, retained_models).await;
        // Built once for the whole tick, outside the lock, so every recipe's ceilings read the same prices and no
        // recipe rebuilds a table's rows.
        let revisions = self.parts.pricing.revisions();
        let rows = FactorRows::of(revisions.in_force(now), &self.parts.tables, now);
        let (jobs, lapsed) = {
            let mut inner = self.lock();
            let inner = &mut *inner;
            if inner.closed {
                return;
            }
            let mut lapsed: Vec<Skipped> = Vec::new();
            inner.warm(|w, s| w.follow_resolutions(s, &view.canonical));
            let mut due: Vec<(i64, SessionKey, String, Holder)> = Vec::new();
            let candidates: Vec<(SessionKey, String)> = inner
                .sessions
                .entries
                .iter()
                .flat_map(|(key, s)| s.agents.keys().map(move |a| (key, a)))
                .filter(|(key, a)| inner.sessions.live(key, a))
                .filter(|(key, a)| {
                    let warming = inner.warming.held(key, a);
                    warming.current.is_some() && warming.compaction.is_none()
                })
                .map(|(key, a)| (key.clone(), a.clone()))
                .collect();
            for (key, agent_id) in candidates {
                if !inner.sessions.live(&key, &agent_id) {
                    continue;
                }
                let warming = inner.warming.held(&key, &agent_id);
                // The ceilings and the gate run for every retained recipe, armed or not: a pause clears the
                // schedule, so an opt-out could otherwise never discard a recipe paused by a 429 and a ceiling
                // could never settle it while the companion keeps reporting. A recipe an open compaction holds
                // waits as it stood for the compaction's completion to keep or stop it.
                if warming.current.is_none() || warming.compaction.is_some() {
                    continue;
                }
                let ready = warming
                    .current
                    .as_ref()
                    .is_some_and(|o| o.ready && o.expected > 0);
                // Before any hold can short-circuit: a recipe held under `horizon` never reaches the gate, and the
                // ceilings below read the pool's policy as it stands now, so a horizon raised in the console
                // reopens the hold and an opt-out discards the prompt the hold retained.
                let refreshed = {
                    let agent =
                        agent_ref(&inner.sessions.entries, &key, &agent_id).expect(AGENT_HELD);
                    let warming =
                        warming_mut(&mut inner.warming.agents, &key, &agent_id).expect(AGENT_HELD);
                    refresh_policy(&view, agent, warming)
                };
                let refreshed = match refreshed {
                    Ok((pool, policy, moved)) => {
                        // Read at every tick, so only a change is written.
                        if moved {
                            inner.touch_agent(&key, &agent_id);
                        }
                        Ok((pool, policy))
                    }
                    Err(Gate::Drop(reason, label)) => {
                        inner.warm(|w, s| w.discard(s, &key, &agent_id, reason, Some(label)));
                        continue;
                    }
                    Err(Gate::Hold(reason)) => Err(reason),
                };
                // Ahead of the refresh's hold, so a deadline or a wake that passes while the pool is disabled is
                // recorded when it passes.
                match inner.sessions.park_passed(&key, &agent_id, now) {
                    ParkPassed::No => {}
                    ParkPassed::Part => {
                        inner.sessions.end_passed_park(&key, &agent_id, now);
                        inner.touch_agent(&key, &agent_id);
                    }
                    ParkPassed::Whole => {
                        // The window restarts here, so the one counted so far ends with no return, closed before its
                        // start moves.
                        inner
                            .warming
                            .close_idle_window(&inner.sessions, &key, &agent_id);
                        inner.warming.clear_idle_floor(&key, &agent_id);
                        inner.sessions.end_passed_park(&key, &agent_id, now);
                        inner.touch_agent(&key, &agent_id);
                    }
                }
                let (pool, policy) = match refreshed {
                    Ok(refreshed) => refreshed,
                    Err(reason) => {
                        if inner.warm_with(|w, s| w.lapse(s, &key, &agent_id, now, &mut lapsed)) {
                            continue;
                        }
                        // A paused recipe keeps its own reason, as under the ceilings' hold below: the pause already
                        // withholds every probe until the next real request.
                        if !inner.warming.held(&key, &agent_id).paused {
                            inner.warm(|w, s| w.hold(s, &key, &agent_id, reason, now));
                        }
                        continue;
                    }
                };
                // The refresh's own hold ends at the first tick that finds the pool enabled again, as the probe verdict
                // below ends `horizon`: nothing else rewrites it before the next probe, and until then an armed recipe
                // would read held with no next probe. A recipe whose request has not brought its evidence yet goes
                // back to waiting for it.
                if inner.warming.held(&key, &agent_id).reason == Reason::PoolDisabled {
                    let resumed = if ready {
                        Reason::Scheduled
                    } else {
                        Reason::AwaitingCacheEvidence
                    };
                    inner.warm(|w, s| w.set_reason(s, &key, &agent_id, resumed));
                }
                let verdict = {
                    let session = &inner.sessions.entries[&key];
                    let agent = &session.agents[&agent_id];
                    let warming = inner.warming.held(&key, &agent_id);
                    let ceilings = ceilings_for(warming, policy, &view, &rows);
                    ceiling(
                        agent,
                        session.attended_at(now),
                        session.last_real_at,
                        &ceilings,
                        now,
                    )
                };
                match verdict {
                    Verdict::IdleLimit => {
                        inner.warm(|w, s| {
                            w.end_observation(
                                s,
                                &key,
                                &agent_id,
                                Some(Wasted::IdleLimit),
                                Unanswered::Keep,
                            )
                        });
                        inner.warm(|w, s| w.set_reason(s, &key, &agent_id, Reason::IdleLimit));
                        continue;
                    }
                    Verdict::ParkedLimit => {
                        inner.warm(|w, s| {
                            w.discard(
                                s,
                                &key,
                                &agent_id,
                                Reason::ParkedLimit,
                                Some(Wasted::ParkedLimit),
                            )
                        });
                        continue;
                    }
                    Verdict::Horizon => {
                        // A held recipe still ends at its entry's expiry: a cold body is worth nothing to the wake.
                        if inner.warm_with(|w, s| w.lapse(s, &key, &agent_id, now, &mut lapsed)) {
                            continue;
                        }
                        let warming = inner.warming.held(&key, &agent_id);
                        let (paused, began) = (warming.paused, warming.reason != Reason::Horizon);
                        // The probes paid so far kept nothing the wake will read, so they are written off as the
                        // hold begins. A paused recipe keeps its own reason and is written off on every held tick,
                        // which finds nothing left after the first.
                        if began || paused {
                            inner.warm(|w, s| {
                                w.settle_unanswered(s, &key, &agent_id, Wasted::Horizon)
                            });
                        }
                        if !paused {
                            inner.warm(|w, s| w.hold(s, &key, &agent_id, Reason::Horizon, now));
                        }
                        continue;
                    }
                    Verdict::Probe => {
                        if inner.warming.held(&key, &agent_id).reason == Reason::Horizon {
                            inner.warm(|w, s| w.set_reason(s, &key, &agent_id, Reason::Scheduled));
                        }
                    }
                }
                if inner.warm_with(|w, s| w.lapse(s, &key, &agent_id, now, &mut lapsed)) {
                    continue;
                }
                let held = gate(
                    &view,
                    &inner.sessions.entries[&key].agents[&agent_id],
                    inner.warming.held(&key, &agent_id),
                    &key.1,
                    pool,
                    policy,
                    now,
                );
                let holder = match held {
                    Err(reason) => {
                        inner.warm(|w, s| w.hold(s, &key, &agent_id, reason, now));
                        continue;
                    }
                    Ok(holder) => holder,
                };
                let warming = inner.warming.held(&key, &agent_id);
                if !ready || warming.in_flight {
                    continue;
                }
                let Some(at) = warming.due_at() else {
                    continue;
                };
                if at <= now {
                    due.push((at, key, agent_id, holder));
                }
            }
            due.sort_by(|a, b| (a.0, &a.1, &a.2).cmp(&(b.0, &b.1, &b.2)));
            let mut jobs = Vec::new();
            for (_, key, agent_id, holder) in due {
                if inner.warming.running >= MAX_WORKERS {
                    break;
                }
                let job = inner.warm_with(|w, s| w.dispatch_probe(s, &key, &agent_id, holder, now));
                // The tracker counts the job from here, under the lock that reserved the worker: the spawn
                // below happens after the lock is released, and a `shutdown` that passes between the two
                // would otherwise find an empty tracker and return while this job was still to come.
                jobs.push((job, self.workers.token()));
            }
            (jobs, lapsed)
        };
        self.flush();
        for skip in lapsed {
            self.record_skipped(now, &skip);
        }
        for (job, token) in jobs {
            let engine = self.clone();
            self.workers.spawn(async move {
                // Bound rather than dropped here: the tracker counts the job while the token lives.
                let _token = token;
                run_job(engine, job).await;
            });
        }
    }

    /// Quiesces, waits for the probes the quiesce canceled, saves, and only then clears: the last save carries
    /// everything the engine held at the quiesce, and a flush already in flight finishes first under the flush lock.
    /// The caller drains the listeners after this, so a probe never outlives the relay it runs through.
    pub async fn shutdown(self: &Arc<Self>) {
        self.quiesce();
        self.workers.close();
        self.workers.wait().await;
        self.flush_persisted().await;
        self.close();
    }

    /// A probe's result, which its worker hands in under the lock. The quiesce is read under the same hold, so a result
    /// that lands behind it changes no recipe the last save carries.
    pub(crate) fn apply_probe(
        &self,
        job: &ProbeJob,
        exchange: Exchange,
        record: &ProbeRecord,
        covered_unit: Option<SavingsUnit>,
        now: i64,
    ) {
        let mut inner = self.lock();
        let inner = &mut *inner;
        // A restart clears every recipe at once and wastes none of them, so the probe settles under no label.
        if inner.closed {
            inner.warming.release_probe_slot(&inner.sessions, job);
            return;
        }
        inner.warm(|w, s| w.apply_probe(s, job, exchange, record, covered_unit, now));
    }
}

impl Engine {
    /// Six asks of the router task per tick, whatever the number of recipes: the scheduler must not queue one
    /// command per agent in front of selection.
    pub(crate) async fn router_view(&self, now: i64, retained_models: Vec<String>) -> RouterView {
        #[cfg(any(test, feature = "debug-hooks"))]
        self.hooks
            .router_view_calls
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let snapshot = self.parts.router.snapshot();
        let canonical = self.dated_names(retained_models).await;
        let pins = self
            .parts
            .router
            .pins()
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|p| {
                (
                    (p.key_id, p.session_id, p.agent_id, p.model),
                    Holder {
                        account_id: p.account_id,
                        incarnation: p.incarnation,
                    },
                )
            })
            .collect();
        let cooled = self
            .parts
            .router
            .cooldowns()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|c| c.until > now)
            .map(|c| ((c.account_id, c.scope), c.window))
            .collect();
        let fallback_ms = self.parts.cooldown_fallback.as_millis() as i64;
        let overage = self
            .parts
            .router
            .overages()
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|(_, e)| e.stands(now, fallback_ms))
            .map(|(id, _)| id)
            .collect();
        let mut spends: BTreeMap<(String, String), Vec<MemberSpend>> = BTreeMap::new();
        for row in self.parts.router.member_spends().await.unwrap_or_default() {
            spends
                .entry((row.pool_id.clone(), row.account_id.clone()))
                .or_default()
                .push(row);
        }
        let budgets = spends
            .into_iter()
            .filter(|((pool_id, account_id), rows)| {
                let rows = rows.iter().collect::<Vec<_>>();
                snapshot
                    .members(pool_id)
                    .iter()
                    .find(|m| &m.account_id == account_id)
                    .is_some_and(|member| budget_deadline(member, &rows, now).is_some())
            })
            .map(|(member, _)| member)
            .collect();
        let holds = self.parts.router.probe_holds().await.unwrap_or_default();
        RouterView {
            budgets,
            canonical,
            cooled,
            credits: holds.credits.into_iter().collect(),
            overage,
            pins,
            policies: pool_policies(&snapshot),
            snapshot,
            spent: holds.spent.into_iter().collect(),
        }
    }

    /// The recipes one session holds, for the suites that ask by ids. Compiled only under the `debug-hooks` feature,
    /// which the crate's own dev-dependency turns on, since no view calls it.
    #[cfg(any(test, feature = "debug-hooks"))]
    pub fn recipes_for(&self, key_id: &str, session_id: &str) -> Vec<String> {
        let inner = self.lock();
        let key = (key_id.to_string(), session_id.to_string());
        inner
            .sessions
            .entries
            .get(&key)
            .map(|s| {
                s.agents
                    .iter()
                    .filter(|(agent_id, _)| inner.warming.held(&key, agent_id).current.is_some())
                    .map(|(_, a)| a.id.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn spawn_scheduler(self: &Arc<Self>) -> tokio::task::JoinHandle<()> {
        let engine = self.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(1));
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tokio::select! {
                    _ = tick.tick() => engine.step().await,
                    _ = engine.cancel.cancelled() => return,
                }
            }
        })
    }
}
