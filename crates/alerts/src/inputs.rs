//! Where the twelve conditions come from: five off the observation bus, six off a one-second poll of state
//! other parts of the process already keep, and one off the lifecycle channel the keepalive engine feeds from
//! the companion's reports.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{
    spawn_subscriber, Bus, Event, Kind as RequestKind, LifecycleReceiver, Origin, Outcome,
    RequestObserved, SessionLifecycle,
};
use phoebis_routing::RouterHandle;
use phoebis_store::{AccountState, AlertEventRow};
use serde_json::json;

use crate::engine::Alerts;
use crate::question::{
    question_in_document, question_in_event, question_text, SseCutter, SSE_BUFFER_MAX,
};
use crate::rule::{Kind, ScopeType, Signal};

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
}

impl AlertInputs {
    pub fn with_tick(mut self, tick: Duration) -> AlertInputs {
        self.tick = tick;
        self
    }
}

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

/// The four kinds the request bus and the companion open and close.
pub const LIFECYCLE_KINDS: [Kind; 4] = [
    Kind::SessionActive,
    Kind::SessionIdle,
    Kind::SessionParked,
    Kind::SubagentActive,
];

/// What the arm keeps about one key and session between that session's requests.
struct SessionState {
    /// The subagents of the session that are still running, by id.
    agents: BTreeMap<String, AgentSeen>,
    /// Turns the session's top-level agent has taken, so a question whose captured body is still being read
    /// can tell whether the user answered while it waited.
    generation: u64,
    /// The newest request or report on any of the session's agents, which the idle rules and the expiry
    /// measure from.
    last_seen: i64,
    /// Whether a `session_parked` event is open, so leaving parked resolves one only where one was opened.
    parked_open: bool,
    /// The pool of the session's key, off the router snapshot, which is what a pool-scoped rule matches.
    pool: Option<String>,
    /// Whether an `ask_user_question` event is open for the pair, so the next turn reads the store only when
    /// there is something there to resolve.
    question_open: bool,
}

/// What a subagent's first request told the arm about it.
struct AgentSeen {
    parent: Option<String>,
}

impl SessionState {
    fn new(now: i64, pool: Option<String>) -> SessionState {
        SessionState {
            agents: BTreeMap::new(),
            generation: 0,
            last_seen: now,
            parked_open: false,
            pool,
            question_open: false,
        }
    }
}

/// The session map the bus arm, the companion's arm and the poller read: a handle, so the three share one map
/// and the enrichment workers that outlive their request keep reaching it.
#[derive(Clone, Default)]
pub struct Tracked(Arc<Mutex<TrackedInner>>);

impl Tracked {
    fn lock(&self) -> std::sync::MutexGuard<'_, TrackedInner> {
        self.0.lock().expect(SESSIONS_POISONED)
    }
}

#[derive(Default)]
struct TrackedInner {
    sessions: HashMap<(String, String), SessionState>,
    /// When a session ended, kept until the expiry: the lifecycle channel and the request bus have consumers
    /// of their own, so an end can be applied before an older request of the session it ended.
    terminals: HashMap<(String, String), i64>,
}

fn pool_of(router: &RouterHandle, key: &str) -> Option<String> {
    router.snapshot().key(key).map(|k| k.pool_id.clone())
}

/// A session is in use: whichever side learned of it says so, and the engine keeps one event per rule and
/// subject, so a repeat costs an insert the unique open index refuses.
fn open_session_active(
    alerts: &Arc<Alerts>,
    key: &str,
    session: &str,
    pool: Option<&str>,
    source: &str,
) {
    alerts.signal(&Signal {
        kind: Kind::SessionActive,
        account_id: None,
        key_id: Some(key.into()),
        pool_id: pool.map(String::from),
        subject: question_subject(key, session),
        detail: json!({ "key": key, "pool": pool, "session": session, "source": source }),
    });
}

fn open_subagent_active(
    alerts: &Arc<Alerts>,
    key: &str,
    session: &str,
    agent: &str,
    parent: Option<&str>,
    pool: Option<&str>,
) {
    alerts.signal(&Signal {
        kind: Kind::SubagentActive,
        account_id: None,
        key_id: Some(key.into()),
        pool_id: pool.map(String::from),
        subject: agent_subject(key, session, agent),
        detail: json!({ "agent": agent, "key": key, "parent_agent": parent, "pool": pool, "session": session }),
    });
}

/// What a session's end or its expiry resolves: the kinds its own subject carries, and one `subagent_active`
/// per subagent still running under it.
fn clear_session(alerts: &Arc<Alerts>, key: &str, session: &str, state: &SessionState) {
    let subject = question_subject(key, session);
    for kind in [Kind::SessionActive, Kind::SessionIdle, Kind::SessionParked] {
        alerts.clear(kind, &subject);
    }
    for agent in state.agents.keys() {
        alerts.clear(Kind::SubagentActive, &agent_subject(key, session, agent));
    }
}

/// Resolves every open lifecycle event of a session past `expiry_ms` since its last request or report, and
/// drops the terminal timestamps older than that; run by the poller on every tick and by the arm before it
/// reads.
pub fn sweep_expired(tracked: &Tracked, alerts: &Arc<Alerts>, expiry_ms: i64, now: i64) {
    let expired: Vec<((String, String), SessionState)> = {
        let mut inner = tracked.lock();
        inner.terminals.retain(|_, ended| now - *ended < expiry_ms);
        let stale: Vec<(String, String)> = inner
            .sessions
            .iter()
            .filter(|(_, s)| now - s.last_seen >= expiry_ms)
            .map(|(pair, _)| pair.clone())
            .collect();
        stale
            .into_iter()
            .filter_map(|pair| inner.sessions.remove_entry(&pair))
            .collect()
    };
    // Outside the lock: a clear reads the open events and posts, which the other two arms must not wait on.
    for ((key, session), state) in expired {
        clear_session(alerts, &key, &session, &state);
    }
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

/// The bus half of the inputs: five of the twelve conditions are facts about a request going by. Public so a
/// test drives it event by event without a bus.
pub struct BusArm {
    alerts: Arc<Alerts>,
    in_flight: HashMap<String, InFlight>,
    router: RouterHandle,
    /// How long a session may go unseen before the arm forgets it. The poller retires that session's open
    /// question at the same age, so nothing the entry is kept for outlives it.
    session_expiry_ms: i64,
    tracked: Tracked,
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
                    state.question_open = true;
                    sessions.insert(pair, state);
                }
            }
            // The process that opened a lifecycle event lost the session maps with it and cannot know whether
            // the session still lives, so the reason travels with the resolve and a live session reopens on
            // its next request.
            for event in open
                .iter()
                .filter(|e| LIFECYCLE_KINDS.iter().any(|k| k.as_str() == e.kind))
            {
                let mut detail = event
                    .detail_json
                    .as_deref()
                    .and_then(|d| serde_json::from_str::<serde_json::Value>(d).ok())
                    .filter(|d| d.is_object())
                    .unwrap_or_else(|| json!({}));
                detail["reason"] = json!("restart");
                let _ = alerts.resolve_with_detail(&event.id, detail);
            }
        }
        BusArm {
            alerts,
            in_flight: HashMap::new(),
            router: inputs.router.clone(),
            session_expiry_ms: inputs.session_expiry.as_millis() as i64,
            tracked: Tracked(Arc::new(Mutex::new(TrackedInner {
                sessions,
                terminals: HashMap::new(),
            }))),
        }
    }

    /// The session map, for the companion's arm and the poller: the three read one map, so a request and a
    /// report about the same session meet.
    pub fn tracked(&self) -> Tracked {
        self.tracked.clone()
    }

    /// How many key-and-session pairs the arm still holds, which is how a test watches the sweep.
    pub fn tracked_sessions(&self) -> usize {
        self.tracked.lock().sessions.len()
    }

    /// Whether the pair's question is claimed, counting the moment the claiming section holds the map and
    /// nobody can read it. Public so a test can act from inside that section.
    pub fn question_claimed(&self, key: &str, session: &str) -> bool {
        match self.tracked.0.try_lock() {
            Ok(tracked) => tracked
                .sessions
                .get(&(key.to_string(), session.to_string()))
                .is_some_and(|s| s.question_open),
            Err(_) => true,
        }
    }

    /// The lifecycle side of a client's request: the session it belongs to, and the subagent that sent it.
    fn observe_lifecycle(&self, tracked: &mut TrackedInner, o: &RequestObserved, now: i64) {
        let (Some(key), Some(session)) = (o.key_id.as_deref(), o.session_id.as_deref()) else {
            return;
        };
        let pair = (key.to_string(), session.to_string());
        // The lifecycle channel and the request bus have consumer tasks of their own, so a session's end can
        // be applied before an older request of that session; the end is the later word.
        if tracked
            .terminals
            .get(&pair)
            .is_some_and(|ended| o.received_at <= *ended)
        {
            return;
        }
        let pool = match tracked.sessions.get(&pair) {
            Some(state) => state.pool.clone(),
            None => pool_of(&self.router, key),
        };
        let state = tracked
            .sessions
            .entry(pair)
            .or_insert_with(|| SessionState::new(now, pool.clone()));
        state.last_seen = now;
        open_session_active(&self.alerts, key, session, pool.as_deref(), "request");
        let agent = o.agent_id.as_deref().unwrap_or(MAIN_AGENT);
        if agent != MAIN_AGENT && !state.agents.contains_key(agent) {
            let seen = state.agents.entry(agent.to_string()).or_insert(AgentSeen {
                parent: o.parent_agent_id.clone(),
            });
            open_subagent_active(
                &self.alerts,
                key,
                session,
                agent,
                seen.parent.as_deref(),
                pool.as_deref(),
            );
        }
        // A request is the session in use, whatever an idle rule decided a moment before it arrived.
        self.alerts
            .clear(Kind::SessionIdle, &question_subject(key, session));
    }

    pub fn observe(&mut self, event: Event) {
        let now = (self.alerts.parts().clock)();
        self.in_flight
            .retain(|_, e| now - e.seen_at < IN_FLIGHT_TTL_MS);
        // An entry must outlive the question event it tracks, so a claim below ages it from when that event
        // opened rather than from the turn that asked, and the poller retires both at the same expiry.
        sweep_expired(&self.tracked, &self.alerts, self.session_expiry_ms, now);
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
                    if client_turn {
                        self.observe_lifecycle(&mut tracked, &o, now);
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
                        Outcome::Exhausted => self.alerts.signal(&Signal {
                            kind: Kind::PoolExhausted,
                            account_id: None,
                            key_id: e.key.clone(),
                            pool_id: Some(pool.clone()),
                            subject: pool.clone(),
                            detail: json!({ "key": e.key, "pool": pool }),
                        }),
                        Outcome::Succeeded => self.alerts.clear(Kind::PoolExhausted, pool),
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
                        if e.capture {
                            // The ledger lands the body a batch after the completion; a short poll reads it. The
                            // session may move on meanwhile, in which case the question was answered before this
                            // signal could open it and it must not open a stale event.
                            let alerts = self.alerts.clone();
                            let id = c.request_id.clone();
                            let tracked = self.tracked.clone();
                            let (pair, admitted) = ((key.clone(), session.clone()), e.generation);
                            tokio::spawn(async move {
                                let mut signal = base;
                                for _ in 0..10 {
                                    let db = alerts.parts().ledger.db().clone();
                                    let request = id.clone();
                                    // The read holds the ledger's reader lock, so it goes off the runtime's
                                    // workers rather than parking one for its duration.
                                    let read =
                                        tokio::task::spawn_blocking(move || db.bodies(&request));
                                    if let Ok(Ok(Some(b))) = read.await {
                                        signal.detail["question"] = json!(b
                                            .response_body
                                            .as_deref()
                                            .and_then(question_text));
                                        // The text is a copy of that body, so it may not outlive it: the
                                        // key's capture window can be far shorter than the global ceiling
                                        // the purge would otherwise hold the question to.
                                        signal.detail["question_expires_at"] = json!(b.expires_at);
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                }
                                let now = (alerts.parts().clock)();
                                // The claim and the signal are one critical section: with the lock released
                                // between them the session's next turn could take the flag and clear a
                                // question whose event has not reached the store yet.
                                let mut tracked = tracked.lock();
                                let state = tracked
                                    .sessions
                                    .entry(pair)
                                    .or_insert_with(|| SessionState::new(now, None));
                                if state.generation != admitted {
                                    return;
                                }
                                state.last_seen = now;
                                state.question_open = true;
                                alerts.signal(&signal);
                            });
                        } else {
                            let mut tracked = self.tracked.lock();
                            let state = tracked
                                .sessions
                                .entry((key.clone(), session.clone()))
                                .or_insert_with(|| SessionState::new(now, None));
                            // The same window the captured path checks: a response completing after the
                            // session's next top-level turn is answering a question that turn already closed.
                            if state.generation == e.generation {
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
        let mut tracked = self.tracked.lock();
        match event {
            SessionLifecycle::AgentReleased {
                key_id,
                session_id,
                agent_id,
            } => {
                let Some(state) = tracked
                    .sessions
                    .get_mut(&(key_id.clone(), session_id.clone()))
                else {
                    return;
                };
                state.last_seen = now;
                if state.agents.remove(&agent_id).is_some() {
                    self.alerts.clear(
                        Kind::SubagentActive,
                        &agent_subject(&key_id, &session_id, &agent_id),
                    );
                }
            }
            SessionLifecycle::AgentStarted {
                key_id,
                session_id,
                agent_id,
            } => {
                let pool = pool_of(&self.router, &key_id);
                let state = tracked
                    .sessions
                    .entry((key_id.clone(), session_id.clone()))
                    .or_insert_with(|| SessionState::new(now, pool));
                state.last_seen = now;
                let pool = state.pool.clone();
                if !state.agents.contains_key(&agent_id) {
                    // A report names no parent: only the client's own header says which agent spawned which.
                    state
                        .agents
                        .insert(agent_id.clone(), AgentSeen { parent: None });
                    open_subagent_active(
                        &self.alerts,
                        &key_id,
                        &session_id,
                        &agent_id,
                        None,
                        pool.as_deref(),
                    );
                }
            }
            SessionLifecycle::MainParked { key_id, session_id } => {
                let Some(state) = tracked
                    .sessions
                    .get_mut(&(key_id.clone(), session_id.clone()))
                else {
                    return;
                };
                state.last_seen = now;
                if !state.parked_open {
                    state.parked_open = true;
                    let pool = state.pool.clone();
                    self.alerts.signal(&Signal {
                        kind: Kind::SessionParked,
                        account_id: None,
                        key_id: Some(key_id.clone()),
                        pool_id: pool.clone(),
                        subject: question_subject(&key_id, &session_id),
                        detail: json!({ "key": key_id, "pool": pool, "session": session_id }),
                    });
                }
            }
            SessionLifecycle::MainResumed { key_id, session_id } => {
                let Some(state) = tracked
                    .sessions
                    .get_mut(&(key_id.clone(), session_id.clone()))
                else {
                    return;
                };
                state.last_seen = now;
                if std::mem::take(&mut state.parked_open) {
                    self.alerts
                        .clear(Kind::SessionParked, &question_subject(&key_id, &session_id));
                }
            }
            SessionLifecycle::SessionEnded { key_id, session_id } => {
                let pair = (key_id.clone(), session_id.clone());
                if let Some(state) = tracked.sessions.remove(&pair) {
                    clear_session(&self.alerts, &key_id, &session_id, &state);
                }
                // The end outlives the entry it removed: a request of this session still on the bus, received
                // before it, must not open the session again.
                tracked.terminals.insert(pair, now);
            }
            SessionLifecycle::SessionStarted { key_id, session_id } => {
                let pool = pool_of(&self.router, &key_id);
                let state = tracked
                    .sessions
                    .entry((key_id.clone(), session_id.clone()))
                    .or_insert_with(|| SessionState::new(now, pool));
                state.last_seen = now;
                let pool = state.pool.clone();
                open_session_active(
                    &self.alerts,
                    &key_id,
                    &session_id,
                    pool.as_deref(),
                    "companion",
                );
            }
        }
    }
}

/// The polled half of the inputs: six of the twelve conditions are state another part of the process already
/// keeps. Public so a test drives one evaluation without waiting for the ticker.
pub struct Poller {
    alerts: Arc<Alerts>,
    failures: BTreeMap<String, u32>,
    last_resolved: (i64, u64),
    question_retention_ms: i64,
    router: RouterHandle,
    session_expiry_ms: i64,
    spent_today: SpendSource,
    ticks: u64,
    tracked: Tracked,
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
                let committed = store
                    .last_refresh_attempt(&event.subject)
                    .ok()
                    .flatten()
                    .is_some_and(|a| a.outcome.as_deref() == Some("committed"));
                if committed {
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
        Poller {
            alerts: alerts.clone(),
            failures: BTreeMap::new(),
            last_resolved,
            question_retention_ms: inputs.body_capture_max_hours as i64 * 3_600_000,
            router: inputs.router.clone(),
            session_expiry_ms: inputs.session_expiry.as_millis() as i64,
            spent_today: inputs.spent_today.clone(),
            ticks: 0,
            tracked,
        }
    }

    pub async fn tick(&mut self) {
        let parts = self.alerts.parts();
        let now = (parts.clock)();
        self.ticks += 1;
        if let Ok(accounts) = parts.store.accounts() {
            // One read for the whole sweep, because the clear below runs on every tick the account is out of
            // the state rather than on the tick it leaves: a clear the store refused once is retried, and a
            // `clear` per account and kind would read the open events again each time.
            let open = parts.store.open_alert_events().unwrap_or_default();
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
                            detail: json!({ "account": attempt.account_id, "failures": *n, "last_error": attempt.detail }),
                        });
                    }
                }
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
                        Some("5h") => sample.five_hour,
                        Some("7d") => sample.seven_day,
                        Some("7d_oi") => sample.fable,
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
                    } else if value < threshold - 0.05 {
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
                let Some(cap) = cap else { continue };
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
                } else {
                    self.alerts.clear(Kind::KeepaliveCostCap, &pool.id);
                }
            }
        }
        // Before the idle rules read the map, so a session with no companion and no further traffic resolves
        // at its expiry rather than at its next request, which may never come.
        sweep_expired(&self.tracked, &self.alerts, self.session_expiry_ms, now);
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
            let idle: Vec<Signal> = {
                let tracked = self.tracked.lock();
                tracked
                    .sessions
                    .iter()
                    .filter(|(_, s)| now - s.last_seen >= idle_ms)
                    .map(|((key, session), s)| Signal {
                        kind: Kind::SessionIdle,
                        account_id: None,
                        key_id: Some(key.clone()),
                        pool_id: s.pool.clone(),
                        subject: question_subject(key, session),
                        detail: json!({ "idle_minutes": threshold, "key": key, "last_request_at": s.last_seen, "pool": s.pool, "session": session }),
                    })
                    .collect()
            };
            // The rule's own scope admits the session or does not, and the engine's one event per rule and
            // subject makes a repeat a no-op, so two rules open at their own minute and neither opens twice.
            for signal in idle {
                self.alerts.signal_rule(&rule.id, &signal);
            }
        }
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

/// The bus arm, the companion's arm where a channel feeds it, and the ticker. All stop when their source
/// closes or the handles are aborted.
pub fn spawn_inputs(
    alerts: Arc<Alerts>,
    mut inputs: AlertInputs,
) -> Vec<tokio::task::JoinHandle<()>> {
    let subscriber = inputs.bus.subscribe("alerts");
    // The arm owns the session map and the other two read it, so it is built first.
    let mut arm = BusArm::new(alerts.clone(), &inputs);
    let tracked = arm.tracked();
    let bus_task = spawn_subscriber(subscriber, move |mut subscriber| async move {
        while let Some(event) = subscriber.events.recv().await {
            arm.observe(event);
        }
    });
    let mut handles = vec![bus_task];
    if let Some(mut lifecycle) = inputs.lifecycle.take() {
        let mut companion =
            LifecycleArm::new(alerts.clone(), inputs.router.clone(), tracked.clone());
        handles.push(tokio::spawn(async move {
            while let Some(event) = lifecycle.recv().await {
                companion.apply(event);
            }
        }));
    }
    let mut poller = Poller::new(alerts, &inputs, tracked);
    let tick = inputs.tick;
    handles.push(tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            poller.tick().await;
        }
    }));
    handles
}
