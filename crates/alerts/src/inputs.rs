//! Where the conditions and the session events come from: five conditions off the observation bus, eight off a
//! one-second poll of state other parts of the process already keep, and the session's events off the bus, the
//! lifecycle channel the keepalive engine feeds from the companion's reports, and the poll's idle and expiry
//! clocks.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{
    spawn_subscriber, Bus, Event, Kind as RequestKind, LifecycleReceiver, Origin, Outcome,
    RequestObserved, SessionLifecycle,
};
use phoebis_ledger::{polled, read_reconciliations, ReconciliationRow, RATIO_HIGH, RATIO_LOW};
use phoebis_oauth::{PollOutcome, UsageService};
use phoebis_routing::{RouterHandle, FIVE_HOUR, SEVEN_DAY, SEVEN_DAY_FABLE};
use phoebis_store::{AccountState, AlertEventRow};
use serde_json::json;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;

use crate::engine::Alerts;
use crate::question::{
    question_in_document, question_in_event, question_text, SseCutter, SSE_BUFFER_MAX,
};
use crate::rule::{Kind, LifecycleEvent, ScopeType, Signal};

pub type SpendSource = Arc<dyn Fn() -> BTreeMap<String, f64> + Send + Sync>;

pub struct AlertInputs {
    /// The ceiling on a captured body's life, which a question detail that carries no deadline of its own
    /// falls back to: the text came out of a body, and outliving it would keep prompt content the operator
    /// asked to be rid of.
    pub body_capture_max_hours: u32,
    pub bus: Bus,
    /// The companion's reports about its sessions; `None` where nothing feeds them, as in a test that drives
    /// the arm by hand.
    pub lifecycle: Option<LifecycleReceiver>,
    pub router: RouterHandle,
    pub session_expiry: Duration,
    pub spent_today: SpendSource,
    pub tick: Duration,
    /// Where the poll's standing per account is read; `None` where a harness builds no usage service, and the
    /// backoff alert then never fires.
    pub usage: Option<Arc<UsageService>>,
}

impl AlertInputs {
    pub fn with_tick(mut self, tick: Duration) -> AlertInputs {
        self.tick = tick;
        self
    }
}

/// How long after a run's first 429 the alert fires, so a single 429 or a short run never alerts.
pub const BACKOFF_ALERT_AFTER_MS: i64 = 30 * 60 * 1_000;

/// How long an in-flight entry may wait for its completion before the arm forgets it.
const IN_FLIGHT_TTL_MS: i64 = 15 * 60 * 1_000;

/// How many ticks between purges of resolved events past their retention.
const PURGE_EVERY: u64 = 3_600;

/// Resolved events are read for the delivery view; ninety days matches the ledger's default retention.
const EVENT_RETENTION_MS: i64 = 90 * 86_400_000;

const SESSIONS_POISONED: &str = "alert session mutex poisoned by a prior panic";

/// The session's top-level agent, whose turns are the session's own; the companion spells it the same way.
const MAIN_AGENT: &str = "main";

/// The idle threshold's bounds, in minutes.
pub const IDLE_MINUTES: std::ops::RangeInclusive<f64> = 1.0..=10_080.0;

/// What the arm keeps about one key and session between that session's requests.
struct SessionState {
    /// The subagents of the session that are still running, by id, with the parent the client named for each,
    /// so a subagent's first request posts its start once and its end names the same parent.
    agents: BTreeMap<String, Option<String>>,
    /// Turns the session's top-level agent has taken, so a question whose captured body is still being read
    /// can tell whether the user answered while it waited.
    generation: u64,
    /// The idle rules already posted for the pair since its last request, by rule id: a crossing is one event
    /// however long it lasts, and the next request empties the set, which arms every rule again.
    idle_posted: BTreeSet<String>,
    /// The newest client request on any of the session's agents, which the idle rules measure from: a report
    /// about the session is not the session being used.
    last_request: i64,
    /// The newest sign of life of any kind, request or report, which the expiry measures from.
    last_seen: i64,
    /// Whether `session_parked` was posted for the pair and not yet followed by `session_resumed`.
    parked: bool,
    /// The pool of the session's key, which is what a pool-scoped rule matches; `None` until a request or a
    /// snapshot lookup says which, and never read as a match.
    pool: Option<String>,
    /// Whether an `ask_user_question` event is open for the pair, so the next turn reads the store only when
    /// there is something there to resolve.
    question_open: bool,
    /// Whether `session_started` was posted for the pair, by a request or by the companion, whichever came
    /// first; the flag is set whether or not a rule matched, so a rule enabled later gets no synthetic start.
    started: bool,
}

impl SessionState {
    fn new(now: i64, pool: Option<String>) -> SessionState {
        SessionState {
            agents: BTreeMap::new(),
            generation: 0,
            idle_posted: BTreeSet::new(),
            last_request: now,
            last_seen: now,
            parked: false,
            pool,
            question_open: false,
            started: false,
        }
    }
}

/// The session map the bus arm, the companion's arm and the poller read, and the order their store work runs
/// in: a handle, so the three share one map and the enrichment workers that outlive their request keep
/// reaching it.
#[derive(Clone, Default)]
pub struct Tracked(Arc<Shared>);

#[derive(Default)]
struct Shared {
    /// Taken before the map and held through the store work or the posts a decision produced, so an open and
    /// a clear reach the store, and a session's events post, in the order the map decided them. The three arms
    /// run on tasks of their own: without it, a decision that raced a session's end could still land after the
    /// end's own work had run, and nothing would ever revisit it once the map entry it belongs to is gone.
    order: Mutex<()>,
    sessions: Mutex<TrackedInner>,
}

impl Tracked {
    /// Hold this across a decision and the emits it produced, and never take it twice on one path: the guard
    /// is a plain mutex, so a second take on the same thread would wait on itself.
    fn ordered(&self) -> std::sync::MutexGuard<'_, ()> {
        self.0.order.lock().expect(SESSIONS_POISONED)
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, TrackedInner> {
        self.0.sessions.lock().expect(SESSIONS_POISONED)
    }
}

#[derive(Default)]
struct TrackedInner {
    /// When an agent was released, by pair and agent, kept and swept with `terminals` for the same reason:
    /// a request of that agent received no later than the release must not start it again.
    released: HashMap<(String, String, String), i64>,
    sessions: HashMap<(String, String), SessionState>,
    /// When a session ended, kept until the expiry: the lifecycle channel and the request bus have consumers
    /// of their own, so an end can be applied before an older request of the session it ended.
    terminals: HashMap<(String, String), i64>,
}

fn pool_of(router: &RouterHandle, key: &str) -> Option<String> {
    router.snapshot().key(key).map(|k| k.pool_id.clone())
}

/// One session event a decision produced while it held the session map, delivered once the map is free: the
/// engine reads the rules and queues the deliveries, and the two arms and the poller that share the map may
/// not wait on it.
struct Emit {
    event: LifecycleEvent,
    signal: Box<Signal>,
}

fn emit_all(alerts: &Arc<Alerts>, emits: Vec<Emit>) {
    for emit in emits {
        alerts.deliver_event(&emit.signal, emit.event);
    }
}

/// A session is in use; `source` is the side that learned of it first.
fn session_active_signal(key: &str, session: &str, pool: Option<&str>, source: &str) -> Signal {
    Signal {
        kind: Kind::SessionActive,
        account_id: None,
        key_id: Some(key.into()),
        pool_id: pool.map(String::from),
        subject: question_subject(key, session),
        detail: json!({ "key": key, "pool": pool, "session": session, "source": source }),
    }
}

fn subagent_active_signal(
    key: &str,
    session: &str,
    agent: &str,
    parent: Option<&str>,
    pool: Option<&str>,
) -> Signal {
    Signal {
        kind: Kind::SubagentActive,
        account_id: None,
        key_id: Some(key.into()),
        pool_id: pool.map(String::from),
        subject: agent_subject(key, session, agent),
        detail: json!({ "agent": agent, "key": key, "parent_agent": parent, "pool": pool, "session": session }),
    }
}

fn session_parked_signal(key: &str, session: &str, pool: Option<&str>) -> Signal {
    Signal {
        kind: Kind::SessionParked,
        account_id: None,
        key_id: Some(key.into()),
        pool_id: pool.map(String::from),
        subject: question_subject(key, session),
        detail: json!({ "key": key, "pool": pool, "session": session }),
    }
}

/// What a session's end posts: `session_ended` with how it ended (`companion` for the channel, `expiry` for the
/// sweep) only when this process posted the session's own start, and one `subagent_ended` per subagent still
/// running under it regardless, since an agent enters the map only through its own start and an entry restored
/// from an open question never holds one. The entry is gone by the time these are delivered, so the caller
/// takes it out of the map first.
fn end_session(key: &str, session: &str, state: &SessionState, source: &str) -> Vec<Emit> {
    let pool = state.pool.as_deref();
    let mut emits = if state.started {
        vec![Emit {
            event: LifecycleEvent::SessionEnded,
            signal: Box::new(session_active_signal(key, session, pool, source)),
        }]
    } else {
        Vec::new()
    };
    emits.extend(state.agents.iter().map(|(agent, parent)| Emit {
        event: LifecycleEvent::SubagentEnded,
        signal: Box::new(subagent_active_signal(
            key,
            session,
            agent,
            parent.as_deref(),
            pool,
        )),
    }));
    emits
}

/// Every session past `expiry_ms` since its last request or report posts `session_ended` and a
/// `subagent_ended` per running subagent, and the terminal timestamps older than that are dropped; run by the
/// poller on every tick and by the arm before it reads. The caller holds the ordering guard and the map and
/// emits what this returns once it has released the map, as every other decision in this module does.
fn sweep_expired(tracked: &mut TrackedInner, expiry_ms: i64, now: i64) -> Vec<Emit> {
    tracked
        .terminals
        .retain(|_, ended| now - *ended < expiry_ms);
    tracked
        .released
        .retain(|_, released| now - *released < expiry_ms);
    let stale: Vec<(String, String)> = tracked
        .sessions
        .iter()
        .filter(|(_, s)| now - s.last_seen >= expiry_ms)
        .map(|(pair, _)| pair.clone())
        .collect();
    let mut emits = Vec::new();
    for pair in stale {
        if let Some(((key, session), state)) = tracked.sessions.remove_entry(&pair) {
            emits.extend(end_session(&key, &session, &state, "expiry"));
        }
    }
    emits
}

/// Every session the map still holds ends at a graceful stop, in the shape the expiry uses: the map is the
/// only record that this process ever posted their starts, so without this a session known from its requests
/// alone would be left open and its first request after the restart would post a second start with no end
/// between them. Sorted, so the ends of a shutdown come out in one order whatever the map's own is.
fn end_tracked(tracked: &mut TrackedInner) -> Vec<Emit> {
    let mut remaining: Vec<((String, String), SessionState)> = tracked.sessions.drain().collect();
    remaining.sort_by(|(a, _), (b, _)| a.cmp(b));
    remaining
        .iter()
        .flat_map(|((key, session), state)| end_session(key, session, state, "shutdown"))
        .collect()
}

struct InFlight {
    agent: Option<String>,
    /// A buffered response's bytes, scanned once as a document at completion; `None` for a stream, and for a
    /// buffered response past the cap, which can no longer be parsed whole.
    body: Option<Vec<u8>>,
    capture: bool,
    /// The session's generation when the request was admitted; the session's next top-level turn moves it.
    generation: u64,
    key: Option<String>,
    pool: Option<String>,
    question: bool,
    seen_at: i64,
    session: Option<String>,
    /// The reassembler for a streamed response; `None` for a buffered one.
    stream: Option<SseCutter>,
}

/// The bus half of the inputs: five of the fourteen conditions are facts about a request going by. Public so a
/// test drives it event by event without a bus.
pub struct BusArm {
    alerts: Arc<Alerts>,
    /// Cancels an enrichment worker still polling past the drain bound; closed workers past this point return
    /// without opening anything.
    cancel: CancellationToken,
    in_flight: HashMap<String, InFlight>,
    /// How long a session may go unseen before the arm forgets it. The poller retires that session's open
    /// question at the same age, so nothing the entry is kept for outlives it.
    session_expiry_ms: i64,
    /// Raised by the teardown while the listeners drain: the arm keeps reading, because a completion landing
    /// during the drain is the only thing that opens or clears its pool's exhaustion and nothing reconstructs
    /// that after a restart, but it starts no session the close is about to end.
    stopping: Arc<AtomicBool>,
    tracked: Tracked,
    /// The enrichment workers spawned to read a captured body's question text, so a drain past its bound can
    /// cancel and join them rather than let one outlive the shutdown.
    workers: TaskTracker,
}

const SUBJECT_SEPARATOR: &str = ", session ";

const AGENT_SEPARATOR: &str = ", agent ";

pub fn question_subject(key: &str, session: &str) -> String {
    format!("{key}{SUBJECT_SEPARATOR}{session}")
}

pub fn agent_subject(key: &str, session: &str, agent: &str) -> String {
    format!("{key}{SUBJECT_SEPARATOR}{session}{AGENT_SEPARATOR}{agent}")
}

/// The key and session a question event names, out of the detail the arm wrote or, when that is gone, out of
/// the subject built from the same two.
fn question_parties(event: &AlertEventRow) -> Option<(String, String)> {
    let detail = event
        .detail_json
        .as_deref()
        .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok());
    if let Some(detail) = &detail {
        if let (Some(key), Some(session)) = (detail["key"].as_str(), detail["session"].as_str()) {
            return Some((key.into(), session.into()));
        }
    }
    let (key, session) = event.subject.split_once(SUBJECT_SEPARATOR)?;
    Some((key.into(), session.into()))
}

impl BusArm {
    pub fn new(alerts: Arc<Alerts>, inputs: &AlertInputs) -> BusArm {
        let now = (alerts.parts().clock)();
        let mut sessions = HashMap::new();
        if let Ok(open) = alerts.parts().store.open_alert_events() {
            // A question opened before this arm existed is still waiting for the user, so the sessions holding
            // one are read back: without them the next turn on such a session would look for nothing to resolve.
            for event in open
                .iter()
                .filter(|e| e.kind == Kind::AskUserQuestion.as_str())
            {
                if let Some(pair) = question_parties(event) {
                    let mut state = SessionState::new(event.opened_at, None);
                    // The entry ages from the event it tracks, so the poller retires both together; the idle
                    // rules measure from the restart instead, because no request of that session has reached
                    // this process and its age says nothing about whether the user is still at the keyboard.
                    state.last_request = now;
                    state.question_open = true;
                    sessions.insert(pair, state);
                }
            }
        }
        BusArm {
            alerts,
            cancel: CancellationToken::new(),
            in_flight: HashMap::new(),
            session_expiry_ms: inputs.session_expiry.as_millis() as i64,
            stopping: Arc::new(AtomicBool::new(false)),
            tracked: Tracked(Arc::new(Shared {
                order: Mutex::new(()),
                sessions: Mutex::new(TrackedInner {
                    released: HashMap::new(),
                    sessions,
                    terminals: HashMap::new(),
                }),
            })),
            workers: TaskTracker::new(),
        }
    }

    /// The session map, for the companion's arm and the poller: the three read one map, so a request and a
    /// report about the same session meet.
    pub fn tracked(&self) -> Tracked {
        self.tracked.clone()
    }

    /// The flag the teardown raises, shared with the handles `spawn_inputs` returns. Public so a test that
    /// drives the arm by hand can put it into the state the drain runs in.
    pub fn stopping(&self) -> Arc<AtomicBool> {
        self.stopping.clone()
    }

    /// The token that cancels an enrichment worker, shared with the handles `spawn_inputs` returns.
    pub fn cancel(&self) -> CancellationToken {
        self.cancel.clone()
    }

    /// The tracker an enrichment worker joins, shared with the handles `spawn_inputs` returns.
    pub fn workers(&self) -> TaskTracker {
        self.workers.clone()
    }

    /// How many key-and-session pairs the arm still holds, which is how a test watches the sweep.
    pub fn tracked_sessions(&self) -> usize {
        self.tracked.lock().sessions.len()
    }

    /// Whether the pair's question is claimed, counting the moment the claiming section holds the map and
    /// nobody can read it. Public so a test can act from inside that section.
    pub fn question_claimed(&self, key: &str, session: &str) -> bool {
        match self.tracked.0.sessions.try_lock() {
            Ok(tracked) => tracked
                .sessions
                .get(&(key.to_string(), session.to_string()))
                .is_some_and(|s| s.question_open),
            Err(_) => true,
        }
    }

    /// The lifecycle side of a client's request: the session it belongs to, and the subagent that sent it.
    /// What it decides is returned rather than signalled, so the session map is not held through the store.
    fn observe_lifecycle(
        &self,
        tracked: &mut TrackedInner,
        o: &RequestObserved,
        now: i64,
    ) -> Vec<Emit> {
        let mut emits = Vec::new();
        let (Some(key), Some(session)) = (o.key_id.as_deref(), o.session_id.as_deref()) else {
            return emits;
        };
        let pair = (key.to_string(), session.to_string());
        // The lifecycle channel and the request bus have consumer tasks of their own, so a session's end can
        // be applied before an older request of that session; the end is the later word.
        if tracked
            .terminals
            .get(&pair)
            .is_some_and(|ended| o.received_at <= *ended)
        {
            return emits;
        }
        let agent = o.agent_id.as_deref().unwrap_or(MAIN_AGENT);
        // The release outlives the agent entry it removed, as a session's end outlives its own: a request of
        // that agent received no later than the release is the run that ended, and starting it again would
        // open a `subagent_active` nothing is left to close.
        let released = tracked
            .released
            .get(&(key.to_string(), session.to_string(), agent.to_string()))
            .is_some_and(|released| o.received_at <= *released);
        let state = tracked
            .sessions
            .entry(pair.clone())
            .or_insert_with(|| SessionState::new(now, o.pool_id.clone()));
        state.last_request = now;
        state.last_seen = now;
        // The pool the request names is the key's pool as of that request: a key moved from one pool to
        // another mid-session is matched by the new pool's rules from its next request on.
        if o.pool_id.is_some() {
            state.pool.clone_from(&o.pool_id);
        }
        let pool = state.pool.clone();
        if !state.started {
            state.started = true;
            emits.push(Emit {
                event: LifecycleEvent::SessionStarted,
                signal: Box::new(session_active_signal(
                    key,
                    session,
                    pool.as_deref(),
                    "request",
                )),
            });
        }
        if agent != MAIN_AGENT && !released && !state.agents.contains_key(agent) {
            state
                .agents
                .insert(agent.to_string(), o.parent_agent_id.clone());
            emits.push(Emit {
                event: LifecycleEvent::SubagentStarted,
                signal: Box::new(subagent_active_signal(
                    key,
                    session,
                    agent,
                    o.parent_agent_id.as_deref(),
                    pool.as_deref(),
                )),
            });
        }
        // A request is the session in use, whatever an idle rule posted a moment before it arrived: the set
        // empties, and every idle rule is armed for the session's next crossing.
        state.idle_posted.clear();
        emits
    }

    pub fn observe(&mut self, event: Event) {
        let now = (self.alerts.parts().clock)();
        self.in_flight
            .retain(|_, e| now - e.seen_at < IN_FLIGHT_TTL_MS);
        // One ordering guard for everything this event decides, so the sweep's ends and the posts below
        // reach the engine in that order and ahead of whatever the other two arms decide next. Held through a
        // handle of its own, so reading the event below still has the arm to itself.
        let tracked = self.tracked.clone();
        let ordered = tracked.ordered();
        // An entry must outlive the question event it tracks, so a claim below ages it from when that event
        // opened rather than from the turn that asked, and the poller retires both at the same expiry.
        let mut emits = sweep_expired(&mut self.tracked.lock(), self.session_expiry_ms, now);
        // The event is read in a call of its own: an arm of it that returns early, as a completion with no
        // request of ours does, must not carry the sweep's ends out of the function with it.
        self.read(event, now, &mut emits);
        // The map is free again: what this event decided is posted now, in the order it was decided.
        emit_all(&self.alerts, emits);
        drop(ordered);
    }

    fn read(&mut self, event: Event, now: i64, emits: &mut Vec<Emit>) {
        match event {
            Event::RequestObserved(o) => {
                let mut generation = 0;
                if let (Some(key), Some(session)) = (&o.key_id, &o.session_id) {
                    // Only the top-level agent's own next turn says the user answered: a keepalive probe
                    // replays the session, a token count precedes a turn, and a sub-agent's turn is the
                    // assistant still working, so none of the three is the answer the question waits for.
                    // The lifecycle side takes every client turn, a subagent's included.
                    let client_turn =
                        o.origin == Origin::Client && o.kind == RequestKind::Inference;
                    let answered = client_turn && o.parent_agent_id.is_none();
                    let pair = (key.clone(), session.clone());
                    let mut tracked = self.tracked.lock();
                    // Nothing starts once the teardown has begun: the close is enqueuing the ends of the
                    // sessions this process knows, and a request that lands during the drain must not open
                    // one behind them or arm an idle rule that will never be read again.
                    if client_turn && !self.stopping.load(Ordering::SeqCst) {
                        emits.extend(self.observe_lifecycle(&mut tracked, &o, now));
                    }
                    // A request the session's end outran has no entry to answer against; the end took it.
                    if let Some(state) = tracked.sessions.get_mut(&pair) {
                        if answered {
                            state.generation += 1;
                            state.last_seen = now;
                            // The lock is held across the clear, as it is across the opening signal on the
                            // other side: a turn that takes the flag between an opener's claim and its insert
                            // would clear an event that is not in the store yet and leave it open until the
                            // expiry.
                            if std::mem::take(&mut state.question_open) {
                                self.alerts
                                    .clear(Kind::AskUserQuestion, &question_subject(key, session));
                            }
                        }
                        generation = state.generation;
                    }
                }
                self.in_flight.insert(
                    o.request_id.clone(),
                    InFlight {
                        agent: o.agent_id.clone(),
                        body: (!o.stream).then(Vec::new),
                        capture: o.body_capture_hours > 0,
                        generation,
                        key: o.key_id.clone(),
                        pool: o.pool_id.clone(),
                        question: false,
                        seen_at: now,
                        session: o.session_id.clone(),
                        stream: o.stream.then(SseCutter::default),
                    },
                );
            }
            Event::ResponseChunk(c) => {
                if let Some(e) = self.in_flight.get_mut(&c.request_id) {
                    // The TTL asks whether a request went silent, so a stream still sending is not idle.
                    e.seen_at = now;
                    if let Some(cutter) = e.stream.as_mut() {
                        for data in cutter.push(&c.body) {
                            e.question = e.question || question_in_event(&data);
                        }
                    } else if let Some(body) = e.body.as_mut() {
                        // A buffered answer is one document, scanned whole at completion under the stream's cap.
                        if body.len() + c.body.len() <= SSE_BUFFER_MAX {
                            body.extend_from_slice(&c.body);
                        } else {
                            e.body = None;
                        }
                    }
                }
            }
            Event::RequestComplete(c) => {
                let Some(mut e) = self.in_flight.remove(&c.request_id) else {
                    return;
                };
                if let Some(body) = &e.body {
                    e.question = question_in_document(body);
                }
                if let Some(pool) = &e.pool {
                    match c.outcome {
                        Outcome::Exhausted => {
                            self.alerts.signal(&Signal {
                                kind: Kind::PoolExhausted,
                                account_id: None,
                                key_id: e.key.clone(),
                                pool_id: Some(pool.clone()),
                                subject: pool.clone(),
                                detail: json!({ "key": e.key, "pool": pool }),
                            });
                        }
                        Outcome::Succeeded => {
                            self.alerts.clear(Kind::PoolExhausted, pool);
                        }
                        _ => {}
                    }
                }
                if let Some(trigger) = &c.client_drift {
                    let version = c.client_version.clone().unwrap_or_else(|| "unknown".into());
                    self.alerts.signal(&Signal {
                        kind: Kind::ClientDrift,
                        account_id: None,
                        key_id: e.key.clone(),
                        pool_id: e.pool.clone(),
                        subject: format!("client {version}"),
                        detail: json!({ "client_version": version, "key": e.key, "trigger": trigger }),
                    });
                }
                if e.question {
                    if let (Some(key), Some(session)) = (e.key.clone(), e.session.clone()) {
                        let base = Signal {
                            kind: Kind::AskUserQuestion,
                            account_id: c.account_id.clone(),
                            key_id: Some(key.clone()),
                            pool_id: e.pool.clone(),
                            subject: question_subject(&key, &session),
                            detail: json!({ "agent": e.agent, "key": key, "question": null, "session": session }),
                        };
                        // A worker starts only before the teardown begins: the drain is about to end every
                        // session, and a question found after that would open an event nobody is left to
                        // answer for. Such a question is left for the next process to find, as one still in
                        // flight at a restart already is.
                        if e.capture && !self.stopping.load(Ordering::SeqCst) {
                            // The ledger lands the body a batch after the completion; a short poll reads it. The
                            // session may move on meanwhile, in which case the question was answered before this
                            // signal could open it and it must not open a stale event.
                            let alerts = self.alerts.clone();
                            let id = c.request_id.clone();
                            let tracked = self.tracked.clone();
                            let cancel = self.cancel.clone();
                            let (pair, admitted) = ((key.clone(), session.clone()), e.generation);
                            self.workers.spawn(async move {
                                let mut signal = base;
                                let mut found = false;
                                for _ in 0..10 {
                                    let db = alerts.parts().ledger.db().clone();
                                    let request = id.clone();
                                    tokio::select! {
                                        _ = cancel.cancelled() => return,
                                        result = db.bodies(&request) => {
                                            if let Ok(Some(b)) = result {
                                                signal.detail["question"] = json!(b
                                                    .response_body
                                                    .as_deref()
                                                    .and_then(question_text));
                                                // The text is a copy of that body, so it may not outlive it: the
                                                // key's capture window can be far shorter than the global ceiling
                                                // the purge would otherwise hold the question to.
                                                signal.detail["question_expires_at"] = json!(b.expires_at);
                                                found = true;
                                            }
                                        }
                                    }
                                    if found {
                                        break;
                                    }
                                    tokio::select! {
                                        _ = cancel.cancelled() => return,
                                        _ = tokio::time::sleep(Duration::from_millis(500)) => {}
                                    }
                                }
                                let now = (alerts.parts().clock)();
                                // The claim and the signal are one critical section: with the lock released
                                // between them the session's next turn could take the flag and clear a
                                // question whose event has not reached the store yet.
                                let mut tracked = tracked.lock();
                                // Only an entry that is still there: a session the companion ended took its
                                // own with it, and a question of it may not put one back.
                                let Some(state) = tracked.sessions.get_mut(&pair) else {
                                    return;
                                };
                                if state.generation != admitted {
                                    return;
                                }
                                state.last_seen = now;
                                state.question_open = true;
                                alerts.signal(&signal);
                            });
                        } else if e.capture {
                            // The worker that would have polled for this body never starts.
                        } else {
                            let mut tracked = self.tracked.lock();
                            let state = tracked.sessions.get_mut(&(key.clone(), session.clone()));
                            // The same window the captured path checks: a response completing after the
                            // session's next top-level turn is answering a question that turn already closed.
                            // An entry the session's end removed is gone for good.
                            if let Some(state) = state.filter(|s| s.generation == e.generation) {
                                state.last_seen = now;
                                state.question_open = true;
                                self.alerts.signal(&base);
                            }
                        }
                    }
                }
            }
            Event::AccountSelected(_) | Event::ResponseHead(_) => {}
        }
    }
}

/// The companion half of the lifecycle inputs, sharing the arm's session map. Public so a test drives it
/// report by report without the channel.
pub struct LifecycleArm {
    alerts: Arc<Alerts>,
    router: RouterHandle,
    tracked: Tracked,
}

impl LifecycleArm {
    pub fn new(alerts: Arc<Alerts>, router: RouterHandle, tracked: Tracked) -> LifecycleArm {
        LifecycleArm {
            alerts,
            router,
            tracked,
        }
    }

    pub fn apply(&mut self, event: SessionLifecycle) {
        let now = (self.alerts.parts().clock)();
        let ordered = self.tracked.ordered();
        let emits = self.decide(event, now);
        // The map is free again: what the report decided is posted now, and the guard keeps it ahead of
        // every decision the other two arms take after it.
        emit_all(&self.alerts, emits);
        drop(ordered);
    }

    /// What the report changes in the session map, and what that leaves to signal once the map is free.
    fn decide(&self, event: SessionLifecycle, now: i64) -> Vec<Emit> {
        let mut tracked = self.tracked.lock();
        match event {
            SessionLifecycle::AgentReleased {
                key_id,
                session_id,
                agent_id,
            } => {
                // Recorded before anything else, and whether or not the agent was ever seen here: the
                // request that started it may still be on the bus behind this report.
                tracked
                    .released
                    .insert((key_id.clone(), session_id.clone(), agent_id.clone()), now);
                let Some(state) = tracked
                    .sessions
                    .get_mut(&(key_id.clone(), session_id.clone()))
                else {
                    return Vec::new();
                };
                state.last_seen = now;
                let Some(parent) = state.agents.remove(&agent_id) else {
                    return Vec::new();
                };
                vec![Emit {
                    event: LifecycleEvent::SubagentEnded,
                    signal: Box::new(subagent_active_signal(
                        &key_id,
                        &session_id,
                        &agent_id,
                        parent.as_deref(),
                        state.pool.as_deref(),
                    )),
                }]
            }
            SessionLifecycle::AgentStarted {
                key_id,
                session_id,
                agent_id,
            } => {
                // A start is the agent running again, so the fence its last release left is spent.
                tracked
                    .released
                    .remove(&(key_id.clone(), session_id.clone(), agent_id.clone()));
                let pool = pool_of(&self.router, &key_id);
                let state = tracked
                    .sessions
                    .entry((key_id.clone(), session_id.clone()))
                    .or_insert_with(|| SessionState::new(now, None));
                state.last_seen = now;
                if pool.is_some() {
                    state.pool = pool;
                }
                if state.agents.contains_key(&agent_id) {
                    return Vec::new();
                }
                // A report names no parent: only the client's own header says which agent spawned which.
                state.agents.insert(agent_id.clone(), None);
                vec![Emit {
                    event: LifecycleEvent::SubagentStarted,
                    signal: Box::new(subagent_active_signal(
                        &key_id,
                        &session_id,
                        &agent_id,
                        None,
                        state.pool.as_deref(),
                    )),
                }]
            }
            SessionLifecycle::Heartbeat { key_id, session_id } => {
                // A sign of life and nothing more: the expiry measures from it, the idle rules never do, so
                // a session that only heartbeats still crosses its idle threshold on time.
                if let Some(state) = tracked.sessions.get_mut(&(key_id, session_id)) {
                    state.last_seen = now;
                }
                Vec::new()
            }
            SessionLifecycle::MainParked { key_id, session_id } => {
                let current = pool_of(&self.router, &key_id);
                let Some(state) = tracked
                    .sessions
                    .get_mut(&(key_id.clone(), session_id.clone()))
                else {
                    return Vec::new();
                };
                state.last_seen = now;
                if current.is_some() {
                    state.pool = current;
                }
                if state.parked {
                    return Vec::new();
                }
                state.parked = true;
                let pool = state.pool.clone();
                vec![Emit {
                    event: LifecycleEvent::SessionParked,
                    signal: Box::new(session_parked_signal(&key_id, &session_id, pool.as_deref())),
                }]
            }
            SessionLifecycle::MainResumed { key_id, session_id } => {
                let Some(state) = tracked
                    .sessions
                    .get_mut(&(key_id.clone(), session_id.clone()))
                else {
                    return Vec::new();
                };
                state.last_seen = now;
                if !std::mem::take(&mut state.parked) {
                    return Vec::new();
                }
                let pool = state.pool.clone();
                vec![Emit {
                    event: LifecycleEvent::SessionResumed,
                    signal: Box::new(session_parked_signal(&key_id, &session_id, pool.as_deref())),
                }]
            }
            SessionLifecycle::SessionEnded { key_id, session_id } => {
                let pair = (key_id.clone(), session_id.clone());
                let emits = match tracked.sessions.remove(&pair) {
                    Some(state) => end_session(&key_id, &session_id, &state, "companion"),
                    None => Vec::new(),
                };
                // The end outlives the entry it removed: a request of this session still on the bus, received
                // before it, must not open the session again.
                tracked.terminals.insert(pair, now);
                emits
            }
            SessionLifecycle::SessionStarted { key_id, session_id } => {
                let current = pool_of(&self.router, &key_id);
                let state = tracked
                    .sessions
                    .entry((key_id.clone(), session_id.clone()))
                    .or_insert_with(|| SessionState::new(now, None));
                state.last_seen = now;
                if current.is_some() {
                    state.pool = current;
                }
                if state.started {
                    return Vec::new();
                }
                state.started = true;
                vec![Emit {
                    event: LifecycleEvent::SessionStarted,
                    signal: Box::new(session_active_signal(
                        &key_id,
                        &session_id,
                        state.pool.as_deref(),
                        "companion",
                    )),
                }]
            }
        }
    }
}

/// The polled half of the inputs: eight of the fourteen conditions are state another part of the process already
/// keeps. Public so a test drives one evaluation without waiting for the ticker.
pub struct Poller {
    alerts: Arc<Alerts>,
    failures: BTreeMap<String, u32>,
    /// The newest `computed_at` of the ledger's reconciliations seen.
    last_reconciled: i64,
    last_resolved: (i64, u64),
    question_retention_ms: i64,
    router: RouterHandle,
    session_expiry_ms: i64,
    spent_today: SpendSource,
    ticks: u64,
    tracked: Tracked,
    usage: Option<Arc<UsageService>>,
}

impl Poller {
    pub fn new(alerts: Arc<Alerts>, inputs: &AlertInputs, tracked: Tracked) -> Poller {
        let store = &alerts.parts().store;
        // A refresh that succeeded while no poller ran must still close the failure it answers, so every open
        // refresh_failed event is checked against its account's newest outcome before the watermark skips it.
        if let Ok(open) = store.open_alert_events() {
            for event in open
                .into_iter()
                .filter(|e| e.kind == Kind::RefreshFailed.as_str())
            {
                // An id with no row left has no refresh to fail. One with a row answers for its current holder
                // only: a previous holder's commit refreshed nothing now.
                let resolved = match store.account(&event.subject) {
                    Ok(None) => true,
                    Ok(Some(account)) => store
                        .last_refresh_attempt(&account.id, account.incarnation)
                        .ok()
                        .flatten()
                        .is_some_and(|a| a.outcome.as_deref() == Some("committed")),
                    Err(_) => false,
                };
                if resolved {
                    alerts.clear(Kind::RefreshFailed, &event.subject);
                }
            }
        }
        // Watermarks start at the newest row, as the ledger mirror's do: nothing before the process existed fires.
        let last_resolved = store
            .newest_resolved_refresh_attempt()
            .ok()
            .flatten()
            .unwrap_or((0, 0));
        let last_reconciled = resolve_drift_at_start(&alerts);
        Poller {
            alerts: alerts.clone(),
            failures: BTreeMap::new(),
            last_reconciled,
            last_resolved,
            question_retention_ms: inputs.body_capture_max_hours as i64 * 3_600_000,
            router: inputs.router.clone(),
            session_expiry_ms: inputs.session_expiry.as_millis() as i64,
            spent_today: inputs.spent_today.clone(),
            ticks: 0,
            tracked,
            usage: inputs.usage.clone(),
        }
    }

    pub async fn tick(&mut self) {
        let parts = self.alerts.parts();
        let now = (parts.clock)();
        self.ticks += 1;
        // One read for the whole tick, because every clear below runs on each tick the condition is off rather
        // than on the tick it lifts: a clear the store refused once is retried, and a `clear` per pool, rule,
        // and account would read the open events again each time. The list is the truth about what is open.
        let open = parts.store.open_alert_events().unwrap_or_default();
        if let Ok(accounts) = parts.store.accounts() {
            for account in &accounts {
                for (kind, state) in [
                    (Kind::ReauthRequired, AccountState::ReauthRequired),
                    (Kind::FundingRequired, AccountState::FundingRequired),
                ] {
                    if account.state == state {
                        self.alerts.signal(&Signal {
                            kind,
                            account_id: Some(account.id.clone()),
                            key_id: None,
                            pool_id: None,
                            subject: account.id.clone(),
                            detail: json!({ "account": account.id, "label": account.label }),
                        });
                    } else if open
                        .iter()
                        .any(|e| e.kind == kind.as_str() && e.subject == account.id)
                    {
                        self.alerts.clear(kind, &account.id);
                    }
                }
            }
        }
        if let Ok(attempts) = parts.store.refresh_attempts_resolved_after(
            self.last_resolved.0,
            self.last_resolved.1,
            500,
        ) {
            for attempt in attempts {
                self.last_resolved = (attempt.resolved_at, attempt.id);
                if attempt.outcome == "committed" {
                    self.failures.insert(attempt.account_id.clone(), 0);
                    self.alerts.clear(Kind::RefreshFailed, &attempt.account_id);
                } else {
                    let n = self.failures.entry(attempt.account_id.clone()).or_insert(0);
                    *n += 1;
                    // One failure is a token endpoint having a bad moment; two in a row is a credential
                    // that is not coming back on its own.
                    if *n >= 2 {
                        self.alerts.signal(&Signal {
                            kind: Kind::RefreshFailed,
                            account_id: Some(attempt.account_id.clone()),
                            key_id: None,
                            pool_id: None,
                            subject: attempt.account_id.clone(),
                            detail: json!({
                                "account": attempt.account_id,
                                "failures": *n,
                                "last_error": attempt.detail,
                                "outcome": attempt.outcome,
                            }),
                        });
                    }
                }
            }
        }
        if let Ok(rows) = parts
            .ledger
            .db()
            .reconciliations_computed_after(self.last_reconciled)
            .await
        {
            // Oldest first, so the newest row of a window decides; one this tick opened is not in `open` yet.
            let mut opened = BTreeSet::new();
            for row in rows {
                self.last_reconciled = self.last_reconciled.max(row.computed_at);
                let subject = drift_subject(&row.account_id, &row.window);
                if row.off() {
                    self.alerts.signal(&drift_signal(&row, subject.clone()));
                    opened.insert(subject);
                } else if inside(row.ratio)
                    && (opened.remove(&subject)
                        || open
                            .iter()
                            .any(|e| e.kind == Kind::MeterDrift.as_str() && e.subject == subject))
                {
                    self.alerts.clear(Kind::MeterDrift, &subject);
                }
            }
        }
        if let Some(usage) = &self.usage {
            let snapshot = self.router.snapshot();
            for account in snapshot.accounts().filter(|a| polled(a.kind, a.state)) {
                // No standing yet, as after a restart before the account's first poll, decides nothing.
                let Some(status) = usage.poll_status(&account.id) else {
                    continue;
                };
                if status
                    .backoff_since
                    .is_some_and(|since| since <= now - BACKOFF_ALERT_AFTER_MS)
                {
                    self.alerts.signal(&Signal {
                        kind: Kind::PollBackoff,
                        account_id: Some(account.id.clone()),
                        key_id: None,
                        pool_id: None,
                        subject: account.id.clone(),
                        detail: json!({
                            "account": account.id,
                            "backoff_since": status.backoff_since,
                            "next_attempt_at": status.next_attempt_at,
                            "rate_limited_in_a_row": status.rate_limited_in_a_row,
                        }),
                    });
                } else if status.last_outcome == PollOutcome::Clean
                    && open
                        .iter()
                        .any(|e| e.kind == Kind::PollBackoff.as_str() && e.subject == account.id)
                {
                    self.alerts.clear(Kind::PollBackoff, &account.id);
                }
            }
            // An id the poll no longer asks about (gone, keyed, disconnected or unpaired) has nothing left to
            // back off from; the disconnect and the delete forget its standing, so the loop above never reaches
            // it.
            for event in open.iter().filter(|e| {
                e.kind == Kind::PollBackoff.as_str()
                    && snapshot
                        .account(&e.subject)
                        .is_none_or(|a| !polled(a.kind, a.state))
            }) {
                self.alerts.clear(Kind::PollBackoff, &event.subject);
            }
        }
        if let Ok(samples) = self.router.samples().await {
            let snapshot = self.router.snapshot();
            // The pools an account belongs to and the keys on them are what a pool or key scope matches
            // against. Both come off the one snapshot, so they are built once per sampled account rather than
            // again for every rule.
            let membership: BTreeMap<&str, (Vec<String>, Vec<String>)> = samples
                .iter()
                .map(|(account_id, _)| {
                    let pools: Vec<String> = snapshot
                        .pools()
                        .filter(|p| {
                            snapshot
                                .members(&p.id)
                                .iter()
                                .any(|m| &m.account_id == account_id)
                        })
                        .map(|p| p.id.clone())
                        .collect();
                    let keys: Vec<String> = snapshot
                        .keys()
                        .filter(|k| pools.contains(&k.pool_id))
                        .map(|k| k.id.clone())
                        .collect();
                    (account_id.as_str(), (pools, keys))
                })
                .collect();
            for rule in self
                .alerts
                .rules()
                .into_iter()
                .filter(|r| r.enabled && r.kind == Kind::UtilizationAbove)
            {
                let Some(threshold) = rule.threshold else {
                    continue;
                };
                for (account_id, sample) in &samples {
                    let value = match rule.window.as_deref() {
                        Some("5h") => sample.reading(FIVE_HOUR).map(|r| r.utilization),
                        Some("7d") => sample.reading(SEVEN_DAY).map(|r| r.utilization),
                        Some("7d_oi") => sample.reading(SEVEN_DAY_FABLE).map(|r| r.utilization),
                        _ => None,
                    };
                    let Some(value) = value else { continue };
                    let Some((pools, keys)) = membership.get(account_id.as_str()) else {
                        continue;
                    };
                    // The signal names the scope that admitted it, so the rule's own `matches` inside
                    // `signal_rule` admits it the same way; an account or any_account scope needs no id.
                    let (key_id, pool_id) = match (rule.scope.kind, rule.scope.id.as_deref()) {
                        (ScopeType::Pool, Some(id)) if pools.iter().any(|p| p == id) => {
                            (None, Some(id.to_string()))
                        }
                        (ScopeType::Key, Some(id)) if keys.iter().any(|k| k == id) => {
                            (Some(id.to_string()), None)
                        }
                        (ScopeType::Pool | ScopeType::Key, _) => continue,
                        _ => (None, None),
                    };
                    let signal = Signal {
                        kind: Kind::UtilizationAbove,
                        account_id: Some(account_id.clone()),
                        key_id,
                        pool_id,
                        subject: account_id.clone(),
                        detail: json!({ "account": account_id, "threshold": threshold, "utilization": value, "window": rule.window }),
                    };
                    if value >= threshold {
                        self.alerts.signal_rule(&rule.id, &signal);
                    } else if value < threshold - 0.05
                        && open
                            .iter()
                            .any(|e| e.rule_id == rule.id && &e.subject == account_id)
                    {
                        self.alerts.clear_rule(&rule.id, account_id);
                    }
                }
            }
        }
        let spent = (self.spent_today)();
        if let Ok(pools) = parts.store.pools() {
            for pool in pools {
                // Read out of the pool's JSON rather than through the keepalive crate's policy type, so the
                // alerts crate does not depend on the engine whose spend it compares against.
                let cap = serde_json::from_str::<serde_json::Value>(&pool.keepalive_json)
                    .ok()
                    .and_then(|v| v["probe_cost_cap_usd_per_day"].as_f64());
                let cap = match cap {
                    Some(cap) => cap,
                    // A cap that was cleared resolves its own open event at the next tick, the same as
                    // crossing midnight does, rather than leaving it open until a day rolls over.
                    None => {
                        if open.iter().any(|e| {
                            e.kind == Kind::KeepaliveCostCap.as_str() && e.subject == pool.id
                        }) {
                            self.alerts.clear(Kind::KeepaliveCostCap, &pool.id);
                        }
                        continue;
                    }
                };
                let used = spent.get(&pool.id).copied().unwrap_or(0.0);
                if used >= cap {
                    self.alerts.signal(&Signal {
                        kind: Kind::KeepaliveCostCap,
                        account_id: None,
                        key_id: None,
                        pool_id: Some(pool.id.clone()),
                        subject: pool.id.clone(),
                        detail: json!({ "cap_usd": cap, "pool": pool.id, "spent_usd": used }),
                    });
                } else if open
                    .iter()
                    .any(|e| e.kind == Kind::KeepaliveCostCap.as_str() && e.subject == pool.id)
                {
                    self.alerts.clear(Kind::KeepaliveCostCap, &pool.id);
                }
            }
        }
        // One ordering guard for the tick's lifecycle half, so the sweep's ends and the idle posts below
        // reach the engine in the order the map decided them.
        let ordered = self.tracked.ordered();
        // Before the idle rules read the map, so a session with no companion and no further traffic resolves
        // at its expiry rather than at its next request, which may never come.
        let expired = sweep_expired(&mut self.tracked.lock(), self.session_expiry_ms, now);
        emit_all(&self.alerts, expired);
        for rule in self
            .alerts
            .rules()
            .into_iter()
            .filter(|r| r.enabled && r.kind == Kind::SessionIdle)
        {
            let Some(threshold) = rule.threshold else {
                continue;
            };
            let idle_ms = (threshold * 60_000.0) as i64;
            // The dispatched rules are recorded under the map, so the crossing is claimed before the guard is
            // released and a tick a second later posts nothing it has already been handed. An entry restored
            // from an open question has no start this process posted, and an idle post for it would announce
            // a session nobody was told began.
            let idle: Vec<Signal> = {
                let mut tracked = self.tracked.lock();
                let mut signals = Vec::new();
                for ((key, session), s) in tracked.sessions.iter_mut() {
                    if !s.started
                        || now - s.last_request < idle_ms
                        || !s.idle_posted.insert(rule.id.clone())
                    {
                        continue;
                    }
                    signals.push(Signal {
                        kind: Kind::SessionIdle,
                        account_id: None,
                        key_id: Some(key.clone()),
                        pool_id: s.pool.clone(),
                        subject: question_subject(key, session),
                        detail: json!({ "idle_minutes": threshold, "key": key, "last_request_at": s.last_request, "pool": s.pool, "session": session }),
                    });
                }
                signals
            };
            // The rule's own scope admits the session or does not: the set says the rule was dispatched for
            // the crossing, and the engine stays the one that decides which rules match it.
            for signal in idle {
                self.alerts
                    .deliver_event_rule(&rule.id, &signal, LifecycleEvent::SessionIdle);
            }
        }
        drop(ordered);
        if let Ok(open) = parts.store.open_alert_events() {
            for event in open.into_iter().filter(|e| {
                e.kind == Kind::AskUserQuestion.as_str()
                    && now - e.opened_at >= self.session_expiry_ms
            }) {
                let _ = self.alerts.resolve(&event.id);
            }
        }
        if self.ticks.is_multiple_of(PURGE_EVERY) {
            let _ = parts.store.purge_alert_events_before(
                now - EVENT_RETENTION_MS,
                now - self.question_retention_ms,
                now,
            );
        }
    }
}

/// A drift event's subject: the account and the window, `5h` or `7d`, since `Alerts::clear` resolves every open
/// event of a kind and subject and a healthy 7-day window must not resolve a 5-hour drift.
fn drift_subject(account_id: &str, window: &str) -> String {
    let short = match window {
        FIVE_HOUR => "5h",
        SEVEN_DAY => "7d",
        other => other,
    };
    format!("{account_id}:{short}")
}

/// The window a drift subject names, spelled as the ledger's rows spell it.
fn drift_window(short: &str) -> Option<&'static str> {
    match short {
        "5h" => Some(FIVE_HOUR),
        "7d" => Some(SEVEN_DAY),
        _ => None,
    }
}

fn inside(ratio: f64) -> bool {
    (RATIO_LOW..=RATIO_HIGH).contains(&ratio)
}

fn drift_signal(row: &ReconciliationRow, subject: String) -> Signal {
    Signal {
        kind: Kind::MeterDrift,
        account_id: Some(row.account_id.clone()),
        key_id: None,
        pool_id: None,
        subject,
        detail: json!({
            "account": row.account_id,
            "observed_rise": row.observed_rise,
            "predicted_share": row.predicted_share,
            "ratio": row.ratio,
            "requests": row.requests,
            "window": row.window,
            "window_end": row.window_end,
        }),
    }
}

/// Resolves every open drift event whose account's newest row of that window reads inside, since a window that
/// completed while no poller ran is otherwise never read, and answers the newest `computed_at`, the watermark.
fn resolve_drift_at_start(alerts: &Arc<Alerts>) -> i64 {
    let parts = alerts.parts();
    let rows = match parts
        .ledger
        .db()
        .read_blocking(|conn| read_reconciliations(conn, i64::MIN))
    {
        Ok(rows) => rows,
        Err(error) => {
            tracing::warn!(%error, "the reconciliations could not be read; drift alerts start from now");
            return (parts.clock)();
        }
    };
    let open = parts.store.open_alert_events().unwrap_or_default();
    for event in open
        .into_iter()
        .filter(|e| e.kind == Kind::MeterDrift.as_str())
    {
        let Some((account_id, window)) = event
            .subject
            .rsplit_once(':')
            .and_then(|(account_id, short)| Some((account_id, drift_window(short)?)))
        else {
            continue;
        };
        // Newest first, so the first row of the account and window is its newest.
        let newest = rows
            .iter()
            .find(|r| r.account_id == account_id && r.window == window);
        if newest.is_some_and(|r| inside(r.ratio)) {
            alerts.clear(Kind::MeterDrift, &event.subject);
        }
    }
    rows.iter().map(|r| r.computed_at).max().unwrap_or(i64::MIN)
}

/// The three input tasks, so the binary's teardown can stop the observers before the keepalive close, drain
/// the lifecycle consumer after it, and abort whatever is left when the drain window closes.
pub struct AlertInputHandles {
    alerts: Arc<Alerts>,
    bus: tokio::task::JoinHandle<()>,
    cancel: CancellationToken,
    lifecycle: Option<tokio::task::JoinHandle<()>>,
    poller: tokio::task::JoinHandle<()>,
    stopping: Arc<AtomicBool>,
    tracked: Tracked,
    workers: TaskTracker,
}

/// How long the drain waits for the companion's consumer, and separately for the enrichment workers, before it
/// gives up and posts the ends of what it still tracks.
const DRAIN_BOUND: Duration = Duration::from_secs(5);

impl AlertInputHandles {
    /// Starts no session and runs no sweep from here on, and stops the poller. Synchronous, so once it
    /// returns nothing that lands during the drain can be posted after a session's end. The bus arm itself
    /// keeps reading until `abort`: a request that completes during the drain still opens or clears its
    /// pool's exhaustion, and the poller's tick reads no completion, so a restart would find neither.
    pub fn stop(&mut self) {
        self.stopping.store(true, Ordering::SeqCst);
        self.poller.abort();
    }

    /// Waits for the lifecycle consumer to post what the engine's close enqueued (its sender is gone, so the
    /// task ends on its own once the queue is empty), then for the enrichment workers still reading a captured
    /// body, cancelling whichever is still polling past its own bound, and only then ends every session still
    /// tracked: a worker's question belongs to the session it was reading for, and ending the session first
    /// would post its close before a question that worker still opens. Bounded so a consumer stuck on a store
    /// write, or a worker stuck on one, cannot hold the shutdown open indefinitely. The handle is polled
    /// through a borrow, so a caller that drops this future keeps the handle and can still abort it.
    pub async fn drain(&mut self) {
        if let Some(lifecycle) = self.lifecycle.as_mut() {
            match tokio::time::timeout(DRAIN_BOUND, &mut *lifecycle).await {
                Ok(Ok(())) => {}
                // A consumer that panicked took the session ends it had not posted with it, which is
                // otherwise silent at shutdown.
                Ok(Err(error)) => {
                    tracing::warn!(%error, "the alerts lifecycle consumer did not finish cleanly")
                }
                Err(_) => lifecycle.abort(),
            }
        }
        self.workers.close();
        if tokio::time::timeout(DRAIN_BOUND, self.workers.wait())
            .await
            .is_err()
        {
            tracing::warn!("an alert enrichment worker was still reading a body past the drain bound; cancelling it");
            self.cancel.cancel();
            let _ = tokio::time::timeout(DRAIN_BOUND, self.workers.wait()).await;
        }
        // After the companion's own ends and the workers have had their say, so a session either of them ended
        // is out of the map by now and takes one end rather than two.
        let ordered = self.tracked.ordered();
        let emits = end_tracked(&mut self.tracked.lock());
        emit_all(&self.alerts, emits);
        drop(ordered);
    }

    /// Aborts all three tasks, for a teardown past its drain or one that ran out of time inside it, and cancels
    /// and closes whatever enrichment workers are left.
    pub fn abort(&mut self) {
        self.stop();
        self.bus.abort();
        if let Some(lifecycle) = &self.lifecycle {
            lifecycle.abort();
        }
        self.cancel.cancel();
        self.workers.close();
    }
}

/// The bus arm, the companion's arm where a channel feeds it, and the ticker. The first and the last stop when
/// aborted; the companion's stops when its channel ends or when aborted.
pub fn spawn_inputs(alerts: Arc<Alerts>, mut inputs: AlertInputs) -> AlertInputHandles {
    let subscriber = inputs.bus.subscribe("alerts");
    // The arm owns the session map and the other two read it, so it is built first.
    let mut arm = BusArm::new(alerts.clone(), &inputs);
    let tracked = arm.tracked();
    let stopping = arm.stopping();
    let cancel = arm.cancel();
    let workers = arm.workers();
    let bus = spawn_subscriber(subscriber, move |mut subscriber| async move {
        while let Some(event) = subscriber.events.recv().await {
            arm.observe(event);
        }
    });
    let lifecycle = inputs.lifecycle.take().map(|mut lifecycle| {
        let mut companion =
            LifecycleArm::new(alerts.clone(), inputs.router.clone(), tracked.clone());
        tokio::spawn(async move {
            while let Some(event) = lifecycle.recv().await {
                companion.apply(event);
            }
        })
    });
    let mut poller = Poller::new(alerts.clone(), &inputs, tracked.clone());
    let tick = inputs.tick;
    let poller = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            poller.tick().await;
        }
    });
    AlertInputHandles {
        alerts,
        bus,
        cancel,
        lifecycle,
        poller,
        stopping,
        tracked,
        workers,
    }
}
