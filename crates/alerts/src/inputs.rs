//! Where the eight conditions come from: three off the observation bus, five off a one-second poll of state
//! other parts of the process already keep.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use phoebis_bus::{spawn_subscriber, Bus, Event, Kind as RequestKind, Origin, Outcome};
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
    pub bus: Bus,
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

/// What the arm keeps about one key and session between that session's requests.
struct SessionState {
    /// Turns the session's top-level agent has taken, so a question whose captured body is still being read
    /// can tell whether the user answered while it waited.
    generation: u64,
    last_seen: i64,
    /// Whether an `ask_user_question` event is open for the pair, so the next turn reads the store only when
    /// there is something there to resolve.
    question_open: bool,
}

impl SessionState {
    fn new(now: i64) -> SessionState {
        SessionState {
            generation: 0,
            last_seen: now,
            question_open: false,
        }
    }
}

/// Shared with the enrichment workers, which outlive the request that spawned them.
type Sessions = Arc<Mutex<HashMap<(String, String), SessionState>>>;

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

/// The bus half of the inputs: three of the eight conditions are facts about a request going by. Public so a
/// test drives it event by event without a bus.
pub struct BusArm {
    alerts: Arc<Alerts>,
    in_flight: HashMap<String, InFlight>,
    /// How long a session may go unseen before the arm forgets it. The poller retires that session's open
    /// question at the same age, so nothing the entry is kept for outlives it.
    session_expiry_ms: i64,
    sessions: Sessions,
}

const SUBJECT_SEPARATOR: &str = ", session ";

pub fn question_subject(key: &str, session: &str) -> String {
    format!("{key}{SUBJECT_SEPARATOR}{session}")
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
        // A question opened before this arm existed is still waiting for the user, so the sessions holding one
        // are read back: without them the next turn on such a session would look for nothing to resolve.
        let mut sessions = HashMap::new();
        if let Ok(open) = alerts.parts().store.open_alert_events() {
            for event in open
                .iter()
                .filter(|e| e.kind == Kind::AskUserQuestion.as_str())
            {
                if let Some(pair) = question_parties(event) {
                    sessions.insert(
                        pair,
                        SessionState {
                            question_open: true,
                            ..SessionState::new(now)
                        },
                    );
                }
            }
        }
        BusArm {
            alerts,
            in_flight: HashMap::new(),
            session_expiry_ms: inputs.session_expiry.as_millis() as i64,
            sessions: Arc::new(Mutex::new(sessions)),
        }
    }

    /// How many key-and-session pairs the arm still holds, which is how a test watches the sweep.
    pub fn tracked_sessions(&self) -> usize {
        self.sessions.lock().expect(SESSIONS_POISONED).len()
    }

    pub fn observe(&mut self, event: Event) {
        let now = (self.alerts.parts().clock)();
        self.in_flight
            .retain(|_, e| now - e.seen_at < IN_FLIGHT_TTL_MS);
        self.sessions
            .lock()
            .expect(SESSIONS_POISONED)
            .retain(|_, s| now - s.last_seen < self.session_expiry_ms);
        match event {
            Event::RequestObserved(o) => {
                let mut generation = 0;
                if let (Some(key), Some(session)) = (&o.key_id, &o.session_id) {
                    // Only the top-level agent's own next turn says the user answered: a keepalive probe
                    // replays the session, a token count precedes a turn, and a sub-agent's turn is the
                    // assistant still working, so none of the three is the answer the question waits for.
                    let answered = o.origin == Origin::Client
                        && o.kind == RequestKind::Inference
                        && o.parent_agent_id.is_none();
                    let pair = (key.clone(), session.clone());
                    let mut sessions = self.sessions.lock().expect(SESSIONS_POISONED);
                    if answered {
                        let state = sessions
                            .entry(pair)
                            .or_insert_with(|| SessionState::new(now));
                        state.generation += 1;
                        state.last_seen = now;
                        generation = state.generation;
                        let open = std::mem::take(&mut state.question_open);
                        drop(sessions);
                        if open {
                            self.alerts
                                .clear(Kind::AskUserQuestion, &question_subject(key, session));
                        }
                    } else {
                        generation = sessions.get(&pair).map_or(0, |s| s.generation);
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
                            let sessions = self.sessions.clone();
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
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                }
                                let now = (alerts.parts().clock)();
                                {
                                    let mut sessions = sessions.lock().expect(SESSIONS_POISONED);
                                    let state = sessions
                                        .entry(pair)
                                        .or_insert_with(|| SessionState::new(now));
                                    if state.generation != admitted {
                                        return;
                                    }
                                    state.question_open = true;
                                }
                                alerts.signal(&signal);
                            });
                        } else {
                            self.sessions
                                .lock()
                                .expect(SESSIONS_POISONED)
                                .entry((key.clone(), session.clone()))
                                .or_insert_with(|| SessionState::new(now))
                                .question_open = true;
                            self.alerts.signal(&base);
                        }
                    }
                }
            }
            Event::AccountSelected(_) | Event::ResponseHead(_) => {}
        }
    }
}

/// The polled half of the inputs: five of the eight conditions are state another part of the process already
/// keeps. Public so a test drives one evaluation without waiting for the ticker.
pub struct Poller {
    alerts: Arc<Alerts>,
    failures: BTreeMap<String, u32>,
    last_resolved: (i64, u64),
    router: RouterHandle,
    session_expiry_ms: i64,
    spent_today: SpendSource,
    ticks: u64,
}

impl Poller {
    pub fn new(alerts: Arc<Alerts>, inputs: &AlertInputs) -> Poller {
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
            router: inputs.router.clone(),
            session_expiry_ms: inputs.session_expiry.as_millis() as i64,
            spent_today: inputs.spent_today.clone(),
            ticks: 0,
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
        if let Ok(open) = parts.store.open_alert_events() {
            for event in open.into_iter().filter(|e| {
                e.kind == Kind::AskUserQuestion.as_str()
                    && now - e.opened_at >= self.session_expiry_ms
            }) {
                let _ = self.alerts.resolve(&event.id);
            }
        }
        if self.ticks.is_multiple_of(PURGE_EVERY) {
            let _ = parts
                .store
                .purge_alert_events_before(now - EVENT_RETENTION_MS);
        }
    }
}

/// The bus arm and the ticker. Both stop when the bus closes or the handles are aborted.
pub fn spawn_inputs(alerts: Arc<Alerts>, inputs: AlertInputs) -> Vec<tokio::task::JoinHandle<()>> {
    let subscriber = inputs.bus.subscribe("alerts");
    let mut arm = BusArm::new(alerts.clone(), &inputs);
    let bus_task = spawn_subscriber(subscriber, move |mut subscriber| async move {
        while let Some(event) = subscriber.events.recv().await {
            arm.observe(event);
        }
    });
    let mut poller = Poller::new(alerts, &inputs);
    let tick = inputs.tick;
    let poll_task = tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            interval.tick().await;
            poller.tick().await;
        }
    });
    vec![bus_task, poll_task]
}
